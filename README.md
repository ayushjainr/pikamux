# Pikamux

**Let your agent find and consult the conversation that already did the work.**

You should not have to remember which conversation knows the answer, find it,
and carry its explanation back. Pika turns your existing Codex, Claude Code,
and OpenCode conversations into a discoverable expert network. The bundled
`agent-convo` skill gives your current agent a way to use it.

- **Find relevant experience.** Expert cards describe a conversation's durable
  scope, current work, topics, and artifacts. Your agent searches this metadata
  before spending quota on a consultation—not every old transcript.
- **Ask before rediscovering.** Your agent selects an exact conversation and
  opens a bounded, read-only side consultation with its inherited context. It
  checks the answer against artifacts before using it; the original conversation
  does not receive the consultation messages.
- **Keep the human in control.** The board brings expert cards and operational
  signals together: see what needs a decision, inspect results, and return to a
  native agent interface with `pika NAME`. Cards enrich the board; provider
  evidence, not a card's prose, determines attention state.

For example, before changing an endpoint, your agent can find the conversation
that built it and ask which earlier constraint still matters. The useful result
is a better-informed change—not merely another generated answer.

[Inspect and reproduce the synthetic example](media/launch/proof/DESIGN.md):
Pika discovered a refund expert card and a side consultation recalled its cited
experiment—after a lost response, a fresh retry ID produced two refunds; retaining
the original ID produced one. The source and four tests are included. This is a
designed demonstration, not a production incident or a claim of model superiority;
the [recorded evidence](media/launch/proof/evidence.json) identifies its
development-preview build.

The current release is **0.5.0a3 (alpha)**. Pika is an independent project, not affiliated with OpenAI,
Anthropic, or OpenCode. It is not an agent runtime or a hosted service.
**Known release gap:** a3 can reject consultations from newer paginated Codex
parents. The source-preview fix is not in that release. See
[compatibility before your first trial](docs/first-consultation.md#release-compatibility).

## Start with one conversation

The agent-hosting node requires **Linux or macOS, tmux, and at least one
supported provider CLI**, already installed and authenticated. Pika's installer
handles its own Python runtime. Windows has an
experimental optional client bridge, not local agent hosting; real Windows
Terminal pairing and attachment are not yet release-verified. Native macOS hosting
is new in this alpha; see [compatibility and limitations](docs/guide.md#install-a-pika-node).

Install the alpha on Mac or Linux; no Python/uv installation is needed:

```bash
curl -fsSL https://github.com/ayushjainr/pikamux/releases/download/v0.5.0a3/install.sh | bash -s -- --version v0.5.0a3
```

It installs Pika user-locally, offers a backed-up shell PATH change and onboarding,
and gives OS-specific tmux instructions if it is missing. It never runs sudo for
you. Read the script first if you prefer; it executes code from this publisher.
See [installation and updates](docs/installing.md) for release bundles, private
SSH installation, and update guarantees. This is a prerelease: do not substitute
GitHub's `latest` URL, which selects stable releases only.

Contributors can use the [source-preview path](docs/installing.md#source-preview)
instead. Do not overwrite an installer-managed copy with a development launcher.

During setup, review the proposed changes to provider hooks and configuration.
The bundled `agent-convo` skill is included in that same approval step for each
installed provider; a separate skill-install command is not required. Existing
instructions are backed up, and externally managed skill symlinks are left alone.
Choose one existing conversation. **Setup does not interview agents or create
missing expert cards.** Follow [your first useful consultation](docs/first-consultation.md)
to make that conversation discoverable, then give your current agent a real task:

> Use the agent-convo skill. Before making this change, find one relevant prior
> conversation through Pika and consult it about constraints I might otherwise
> need to explain. Check its cited evidence and tell me what changed in your plan.

You do not need a fleet of servers or an expert panel. Start with one project and
one useful consultation. Discovery makes no model calls; building a missing card
by interview and consulting an expert use provider quota.

For the human view, open the board or return by name:

```bash
pika research-notes
pika
```

On the board, use arrows or `j`/`k` to select, Enter to open, `/` to filter,
and `q` to leave. Detach with **Ctrl-b d** to return to the board; the agent
keeps running. If a live agent is outside
Pika's home, Pika explains the safe handover rather than moving it forcibly.
Codex users must review and trust the installed hooks through `/hooks`.

Installer-managed boards check for updates in the background. When an update is
available, press `U` to review it, then Enter to approve installation. Agents stay
running. `pika update` also works from the shell, including for alpha releases.
For 0.5.0a2 or earlier, run the installer above once to gain this update flow.

Pika is distributed through [GitHub Releases](https://github.com/ayushjainr/pikamux/releases),
not PyPI. Older Git tags do not include this alpha's changes.

## What your agent does

```bash
pika experts "release process"
pika experts "release process" --json
```

From those results, the agent selects a provider and immutable conversation ID,
not just a similar name. It opens `pika ask <native-id> --jsonl`, sends a focused
question, checks the answer, and closes the side. Remote targets retain their
`@machine` route. The [first-consultation guide](docs/first-consultation.md)
shows the exchange and what counts as success. You can also ask interactively
with `pika ask <native-id>`; `/close` finishes that side.

Setup installs the version-matched `agent-convo` skill. `pika skill install`
remains available for manual installation or a custom destination; see the
[agent workflow](src/pikamux/skills/agent-convo/SKILL.md).

Expert profiles can be published by an agent or generated by an explicit
interview. Missing profiles are not evidence that no relevant conversation
exists. Consultations use provider quota and have provider-specific isolation
and cleanup limits; [read those limits](SECURITY.md) before using sensitive work.

## Trust and operating boundaries

Ordinary inventory and attention routing add no model calls. Consultation,
explicit expert interviews, and the existing quota-gated expert refresher can
consume provider quota. Setup previews the refresher configuration and does not
interview agents itself.

Pika stores operational metadata and expert profiles locally. Those can contain
sensitive names, paths, and agent-published summaries. "Private consultation"
means a separate conversation, **not offline processing**: your provider still
processes the consultation. OpenCode side forks are temporary persisted sessions
until verified cleanup. A cleanup failure is reported, not called discarded.

Exact recovery is not disaster recovery: keep provider histories and the host
available. An unreachable machine supplies last-known metadata, not live proof.
Provider CLI updates can change integration behavior; unsupported capabilities
fail closed. Do not rely on a successful demo as a universal safety guarantee.

## Explore without connecting an agent

This static fixture uses invented projects and makes no provider calls:

```bash
python -m pikamux.monitor --demo --width 120 --height 30
```

Run it from an environment with Pika installed (contributors can use
`uv run python -m pikamux.monitor --demo`). It is a synthetic preview, not a
recording of a real consultation.

## Documentation and development

- [Operating guide](docs/guide.md): recovery, experts, SSH machines, and the Windows bridge.
- [First useful consultation](docs/first-consultation.md): one card, one discovery, one verified answer.
- [Architecture](DESIGN.md): identities, ownership, and trust boundaries.
- [Contributing](CONTRIBUTING.md): one development setup and the complete test suite.
- [Security and privacy](SECURITY.md): data boundaries and safe issue reporting.
- [Release checklist](docs/releasing.md): verification before any public release.

Built and maintained by [Ayush Jain](https://github.com/ayushjainr), with
AI-assisted development. Contributions are welcome; please include a small
reproduction and tests for the behavior you change.

Licensed under [MIT](LICENSE).
