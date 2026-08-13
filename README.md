# Pikamux

Pikamux provides the `pika` command: a persistent identity keeper and attention
router for Codex and Claude conversations running in tmux.

Pika does not do the agent's work. It remembers exactly where that work lives
and brings it back when called.

## Why Pika

Agent conversations already persist, and tmux sessions already persist. The
missing piece is a trustworthy bridge between a human name, the provider's
immutable conversation UUID, and the pane where that conversation is running.
Pika owns that bridge.

- It resumes exact UUIDs. It never falls back to `--last` or `--continue`.
- It refuses to open a second copy of a conversation already running elsewhere.
- It routes attention using `NEEDS YOU`, `WORKING`, `READY`, `PARKED`,
  `UNBOUND`, `OPEN TWICE`, and `ERROR` states.
- It stores metadata only. Ordinary views never index or display transcript
  contents; `peek` explicitly captures the visible tmux pane.
- It uses hooks plus command-time reconciliation, with no background daemon.

## Install

Pikamux requires Linux, Python 3.10 or newer, and tmux. Codex and/or Claude must
already be installed.

```bash
uv tool install --editable /path/to/pikamux
pika setup
```

`pika setup` is presented as a commissioning flow, not a blind installer. It
states the integration contract, shows a diff before changing anything, backs up
existing files, preserves existing JSON key order, and merges lifecycle hooks
into Codex and Claude user settings. It offers to adopt existing resumable
conversations with explicit names plus any conversation that is currently live
before asking for a default provider. Archived sessions, missing histories, and
AI-generated Claude summaries stay out of this commissioning choice. For Codex,
setup uses the effective saved name exposed by Codex: names created with
`/rename` are eligible even when the internal SQLite `threads.name` column is
empty. Nothing is imported silently. The final
commissioning ledger distinguishes active hook definitions from observed live
events. Codex requires one extra trust step: open
`/hooks`, approve the Pika definitions, and use Codex once so `pika doctor` can
observe a real lifecycle event. Pika uses “commissioned” only when both
integrations are currently active and have delivered the current hook
definition; otherwise it names every remaining activation or observation proof.

For automation, review a dry run and then apply it explicitly:

```bash
pika setup --dry-run --default-provider codex
pika setup --yes --default-provider codex --no-import
pika setup --yes --default-provider codex --import-all
```

Backups are written beside each changed file with a
`.pika-backup-YYYYMMDDTHHMMSSZ` suffix. Restore one by copying it back over its
original file; Pika never deletes backups.

## Daily commands

```text
pika                       open the live operations monitor
pika NAME                  open or resurrect an exact conversation
pika open NAME             unambiguous form, including `pika open list`
pika ask NAME "QUESTION"  ephemeral multi-turn consultation with that parent
pika ask NAME --jsonl      persistent JSON-lines side channel for agents/apps
pika experts QUERY         find provenance-bound experts across projects
pika experts QUERY --json  stable machine-readable expert matches
pika expert publish ...    publish this exact pane's own expert card
pika expert clear          remove this exact pane's expert card
pika .                     open the relevant conversation for this repository
pika -                     return to the previously attached Pika conversation
pika list                  show all tracked live and parked conversations
pika list --json           stable machine-readable inventory
pika next                  open the oldest conversation needing attention
pika peek NAME             inspect recent pane output without attaching
pika peek NAME --ack       explicitly acknowledge READY in a script
pika wait NAME             wait for NEEDS YOU, unread READY, OPEN TWICE, or ERROR
pika new NAME              create using the configured default provider
pika new NAME --agent ...  override with codex or claude
pika adopt [TMUX_TARGET]    tag an already-running agent pane
pika doctor                print a recoverability receipt
pika doctor --verbose      show every receipt check
pika doctor --repair-stale remove confirmed stale launch locks (5m+)
```

`pika ask master_quant "What assumption is weakest here?"` opens a temporary
side conversation based on that exact provider UUID. In a terminal, ask
follow-ups at the `side>` prompt and type `/close` when finished. Codex uses an
in-memory ephemeral fork. Claude uses one streamed, non-persistent fork with its
tool surface disabled. The parent can keep working; the side is read-only, does
not append to the parent transcript, and is discarded on close. Claude support
is capability-gated to the tested CLI version, and Pika fails closed if it
cannot verify support.

`pika expert publish --summary "..." --topic "..." --artifact "..."` lets a
conversation advertise specific firsthand work. Publication is allowed only
from the conversation's exact UUID-bearing Pika pane; one agent cannot write
another agent's card. `pika experts "factor attribution" --json` ranks the
self-published cards deterministically from topics, summary, project, and
artifacts and exposes `matched_on` rather than pretending the score measures
intelligence. Archived conversations disappear from lookup with the rest of
Pika's daily surface.

Agent and dashboard clients can keep a genuine multi-turn side open with
`pika ask UUID --jsonl`. Send one `{"question":"..."}` object per line and end
with `{"close":true}`. Responses identify the exact parent UUID and explicitly
confirm the discarded ephemeral lifecycle. The same provider process handles
all questions until close, so follow-ups retain side context without replaying
answers or modifying the parent.

The interactive `pika` monitor refreshes operational state every two seconds.
Use arrows or j/k to select, Enter to open the selected identity, `n` for the
oldest attention item, `p` for a sanitized pane peek, `u` to reveal or hide
provider usage, `r` to reconcile immediately, `?` for keys, and `q` to leave.
Usage collection starts separately every thirty seconds only while its view is
visible, so it cannot delay operational updates, opening a workstream, or leaving
the monitor. Selection is stable by provider UUID even when a status change
reorders rows.

The monitor opens with a decision briefing rather than raw process totals. A
first handoff describes current state; after six hours away, a temporary
`SINCE YOUR LAST VISIT` strip counts actionable lifecycle events from Pika's
local transcript-free event ledger using an atomic committed-event watermark.
Quiet screens say `NO ATTENTION PENDING`;
partial or failed reconciliation says `PARTIAL` or `STALE` rather than claiming
complete synchronization. State time is semantic: waiting/results/failures use
their lifecycle event, while working and parked rows report last activity.

The `PIKA PLAYBOOK` strip rotates every five minutes through actions relevant to
the current screen. Exact/protected language appears only when Pika has current
provider UUID-to-PID evidence for that tmux pane. Opening an exact unread result
atomically clears that event and emits a one-shot `RESULT COLLECTED` receipt with
the remaining result count; losing a race to a newer event leaves it unread.
When stdout is redirected, bare `pika` falls back to the finite static briefing;
`pika list --json` remains the preferred automation contract.

When multiple conversations share a name, Pika displays a numbered chooser with
provider, short immutable UUID fingerprint, repository, branch, recency, and
state. Cross-provider collisions explicitly say that both Codex and Claude have
the name; `q`, Esc, or EOF cancels without opening anything. A missing name
prints close matches and never creates a conversation implicitly.

`pika list` opens with a compact delegation briefing. Its `WHY` field is a
closed, transcript-free event reason such as `permission`, `question`,
`completed`, `failed`, or `exited`; `VIEW` means that exact pane is currently
visible, not merely that its tmux session has a client. The wide view includes
process-tree CPU/RAM and provider usage where structured counters are available.
Dollar values are visibly labeled `~API$`: a dated, best-effort API-equivalent
list-price estimate, never a subscription bill. Unknown models or unavailable
usage render as `—` rather than zero. Applicable action hints teach `pika next`,
`pika .`, and `pika -` without adding new workflow concepts.

Inside tmux, `pika peek NAME` uses a popup: Enter attaches, while Esc or `q`
returns. A human terminal view acknowledges an unread `READY`; redirected or
scripted output preserves unread state unless `--ack` is explicit. In scripts,
`pika wait NAME --for needs-you --timeout 600 --json` provides a daemon-free
synchronization primitive.

Pika-created homes enable tmux mouse handling only for that session, so a wheel
or trackpad scroll enters pane history without a prefix key. Scroll back down to
the bottom or press `q` to return to the live agent; hold Shift while dragging
when the outer terminal should select text itself. Newly created and exact-
respawned Pika panes retain up to 100,000 lines. A live pane created by an older
Pika release gains wheel scrolling immediately and the deeper allocation on its
next exact respawn. Global tmux options and adopted user-owned sessions are not
changed.

## Data and recovery

Pika stores configuration in `~/.config/pika/config.json` and owner-only state
in `~/.local/state/pika/pika.db`. The durable key is `(provider, UUID)`; Pika
refreshes provider-native names and may therefore show collisions. A collision
always produces a chooser (or an error in a non-interactive process), never an
implicit provider choice.

Live hook ownership is bound to both a PID and its Linux process start time, so
a recycled PID cannot counterfeit exact-UUID identity. A shared Codex app-server
owner is a five-minute, hook-renewed lease rather than permanent hard identity;
an expired claim is removed automatically when native UUID process evidence
proves the exact pane. A real second UUID-bearing process remains fail-closed and
is displayed as `OPEN TWICE`. Legacy owner rows without start-time evidence
remain fail-closed.

Each Pika-managed conversation gets one UUID-derived tmux session. A new Claude
conversation is named natively with `--name`; a new Codex conversation is named
through Codex's local app-server thread API after SessionStart reveals its UUID.
Pika disables the tmux status bar only for sessions it creates, keeping the
Codex or Claude interface visually unchanged while leaving global tmux settings
and explicitly adopted sessions untouched. It also launches the agent with the
caller's `PATH`, a 24-bit RGB tmux terminal contract, and without stale
automation-only `NO_COLOR` state inherited from an older tmux server. Explicit
interactive `NO_COLOR` preferences remain respected. Pika declares RGB support
to tmux and uses the `tmux-direct` terminfo contract so both agents retain their
24-bit color palettes. Codex additionally derives its adaptive user-message and
composer fills from OSC 10/11 terminal queries, which tmux consumes without
answering. Before starting Codex, Pika queries the directly attached terminal
once and passes the result to a transparent private-PTY bridge. The bridge
answers only those two Codex probes and forwards every other terminal byte; it
does not hard-code a theme or alter Claude's launch path. If the outer terminal
does not report a palette, Pika leaves Codex's conservative fallback unchanged.

The provider process may exit while the tmux session remains as an idle shell.
Opening that conversation later respawns the exact UUID in the same pane. If
that pane contains any foreground or background work, Pika preserves it, clears
its Pika ownership tags, and creates a fresh UUID-derived home instead. If the
tmux session itself disappeared, Pika creates another and resumes the UUID there.
Every successful attach displays a short threshold receipt in tmux: provider,
name, UUID fingerprint, and a proof-scoped outcome such as `ATTACHED EXACT`,
`RESUMED EXACT`, `NEW HOME · EXACT`, or `IDENTITY PENDING`. When work was
preserved, the receipt identifies the old pane and command rather than hiding
the safety decision.

`pika doctor` is deliberately strict. “Safe to close this terminal” requires
valid provider UUIDs, durable provider history (or an exact live tagged pane),
existing saved directories, owner-only state, one owner per identity, installed
hooks, and observed Codex hook trust when Codex is in use. A warning is never
reported as safe.

A successful human receipt is deliberately scoped: `Recovery verified` includes
the exact conversation count and UTC time, followed by `Safe to disconnect this
terminal. Keep the tmux server running.` Inside tmux it also reminds the user how
to detach. Warnings and errors suppress all verified/safe language.

Interrupted launches and resume locks remain visible in verbose and JSON doctor
receipts, including their token, age, pane, and lock-owner PID. `pika doctor
--repair-stale` removes only state older than five minutes: a resume lock whose
recorded PID and process start time prove its owner is gone, or a pending launch
with no matching provider process found by pane ID, tmux session, or launch-token
tag. Active and unprovable legacy state remains fail-closed.

Pika-created and explicitly adopted sessions have the strongest recovery
guarantee. Historical discovery is intentionally best-effort because provider
local indexes and transcript formats may change.

Pika stores identity and operational metadata, not prompts or responses.
Attention reasons come only from closed provider lifecycle event types, never
transcript inference. Usage statistics read only structured provider counters.
`peek` is the explicit exception: it shows already-visible terminal output from
the selected tmux pane.

## Development

The package uses only the Python standard library at runtime.

```bash
python3 -m unittest discover -s tests -v
PYTHONPATH=src python3 -m pikamux --help
```

The integration suite uses an isolated tmux socket and fake provider processes;
it does not touch a developer's normal tmux server or provider configuration.
