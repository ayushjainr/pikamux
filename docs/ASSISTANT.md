# Pika assistant

The assistant keeps its own scoped memory, decisions, and conversation history.
It is separate from project agents and from the board's ephemeral `a`
consultation. This guide describes the current implementation, not an installed
release, a live-usefulness result, or permission to activate it.

## Normal entry

On a macOS/Linux authority host:

```text
pika pika
pika pika --offline
pika pika --scope project-a
pika pika --json
pika pika --scope project-a --remember "Keep comparisons on the same dataset."
pika pika --scope project-a --decision "Use snapshots because reproducibility matters."
```

`P` on the board opens the main assistant beside the live workstream rail on
wide terminals; narrow terminals focus its panel beneath the board header.
Type normally and press Enter to talk; Esc/F12 returns. A selected task is
focus, not permission to read it or a choice of assistant authority. A project
conversation named `pika` remains reachable with `pika open pika`.

The default scope is the saved startup scope, or `personal` when unconfigured.
An exact `--scope` selects a memory namespace;
it does not grant access to a project directory or its conversations. Views of
one state root share one locked host and immutable profile ID. `--json` reports
that profile and current state; it may start the local host, but does not itself
enable a provider. A previously approved, unexpired background lifetime is
independent of these local commands.

A local preview can reuse an existing profile in place with
`pika pika --profile-root /absolute/private/profile --expected-profile-id UUID`.
Both options are required together. Missing, linked, or mismatched profiles are
refused before starting a host or enabling a provider; no replacement profile or
credential copy is made. Operational observation still uses the configured Pika
state/configuration paths, so preview launchers must isolate those separately.

Without an enabled provider, ordinary text is saved as a draft, not sent.
With a ready enabled provider, ordinary text submits a paid reasoning turn.
`/help` lists the interactive commands.

### Feedback during daily use

Use `/feedback Your note` whenever you want something in Pika to change.
Pika appends your exact words with the UTC date and current scope to
`user_feedback.md` in this assistant's private profile directory. Bare `/feedback`
shows its bundled Pika-only skill and the exact file location. The command also
works without an enabled model and during provider recovery. It confirms only a
successful file write; a logged request is not a completed fix.

This is a separate product-feedback log for later review, not assistant memory
or a standing instruction. It captures no surrounding transcript, makes no model
call, and is not automatically retrieved or processed by maintenance. Keep the
file with the profile when backing up feedback; `/forget` applies to assistant
memory, not this explicitly saved file.

### Immediate preferences

In an enabled conversation, a complete instruction such as “From now on daily
briefs should have three bullets” is saved directly as a standing presentation
preference in the current scope. “Going forward” also works, with a count from
one to ten. This exact-wording recognizer adds no model call; your normal answer
still uses its ordinary turn. A later recognized count replaces the old count
in the same scope, without discarding the history. Relevant standing guidance
can be retrieved after a fresh provider context even when newer instructions
have displaced it from the recent-memory shortlist.

This deterministic recognizer is narrow; the ordinary turn's bounded learning
channel handles other supported adaptations as described below. One-off requests,
quotations and ambiguous wording do not become standing instructions.
`/remember` and `/correct` remain available for explicit guidance.
`/forget ID` on a recognized count removes its revision chain and
dependent records so an older count cannot reappear; forgetting is not undo.
The native `/brief` archive view is not a model-written daily brief and is not
reformatted by this preference. Periodic Reflection requires its separate opt-in.

## Local memory, decisions, and briefings

These commands do not request model calls:

| Command | Result |
| --- | --- |
| `/remember TEXT` | Save an explicit instruction in this scope. |
| `/decision TEXT` / `/propose-decision TEXT` | Record an accepted decision / an unaccepted proposal. |
| `/decision-json JSON` | Save structured human decision wording. Fields: `chosen`, `rationale`, `owner`, optional `rejected`, `open_questions`, `commitments` arrays. |
| `/decision-state ID STATE` | Record an explicit proposed, accepted, rejected, deferred, unresolved, or superseded transition. |
| `/decision-revise-json ID JSON` | Create an immutable structured revision; preserve previous wording and provenance. |
| `/why ID` | Inspect historical rationale and authorship, not proof of current project state. |
| `/correct ID TEXT` | Supersede wording in the same scope. Correcting a finding does not turn it into a user instruction or approval. |
| `/explain ID JSON` | Record `choice`, `why`, `alternative`, `remaining_question`; identify missing explanation fields without cognitive scoring. |
| `/skip REASON` / `/defer REASON` | Record an optional understanding exercise's disposition; never pause a project. |

An accepted decision is not an execution receipt. Saving or revising it does
not run project work, and an assistant's proposed rationale is not your approval.

`/brief` shows saved changes since the last **acknowledged** briefing, alongside
standing decisions/instructions and coverage limits. Every update on its page
has an ID and bounded preview. Truncated wording, omitted standing context, and
additional pages are not a complete account of the archive.

`/brief-seen` acknowledges the exact briefing opened in this view. It does not
acknowledge newer arrivals or mark source tasks read. If more changes remain,
open `/brief` again after acknowledging. Viewing alone does not advance the
cursor. A stale cursor may require a fresh briefing.

For inspection and export:

- `/memory` and `/memory-next` page through scoped records, including historical
  revisions. This archive cursor is separate from briefing acknowledgement.
- `/memory-search QUERY` performs bounded lexical/BM25 search. A limited or empty
  result is not proof that the archive contains no other relevant evidence.
- `/memory-record ID` and `/memory-record-next` provide bounded chunks of the
  complete record JSON. Previews are not lossless exports; use these chunks for
  exact wording. Pagination freezes its revision ceiling; start `/memory` again
  for later arrivals. Forgetting invalidates old cursors.

Output stays in the local view/response. There is no model-selected export path,
automatic file export, or permission to read a transcript merely because its
path appears in stored metadata.

## Enable reasoning and sharing separately

To make an existing, already authenticated assistant the normal local entry,
choose it once:

```text
pika pika --profile-root /absolute/profile --expected-profile-id UUID --scope pika --enable-codex /absolute/codex --no-call-limit --set-default
```

Normal `pika pika` and board `P` then reuse that profile, scope and provider.
The selection lives in private `assistant-startup/selection.json` beneath Pika's
state directory. No memory, login or conversation is copied. Missing or mismatched
profiles fail instead of creating a replacement. Existing matching hosts are
reused. `--offline`, `--json`, note-saving and a different explicit scope do not
enable the saved provider. Offline does not stop a provider another view owns.
Selection grants neither background work nor additional source access.

The main adapter is Codex. Before real use, the operator must explicitly choose
the authority/profile, authenticate its isolated `assistant/provider-home`, and
approve the data and call allowance. Pika does not copy ordinary Codex credentials
or provision a login. The adapter checks its restricted effective configuration;
unsupported isolation/readiness fails closed.

An explicit foreground enablement looks like:

```text
pika pika --scope project-a --enable-codex /absolute/path/to/codex --max-calls 8
```

The allowance is a durable lifetime call ceiling, not a dollar limit or a fresh
allowance on each restart. Use `--no-call-limit` instead of `--max-calls N` to
explicitly remove this lifetime ceiling. This retains usage history (including
unknown deliveries), provider quota, per-request bounds, concurrency limits and
deadlines; it does not enable background work. Internally the uncapped setting
uses SQLite's largest integer allowance, without resetting the ledger.
Monetary cost can remain unknown. A normal question
uses the main turn first; `/investigate QUESTION` permits bounded evidence-gap
work, with zero to two helpers when needed and at most four calls including
synthesis. Workers cannot increase their parent's allowance or recursively
spawn more workers.

Board metadata sharing is denied by default. `/board-share` previews exact
names and node/provider/conversation IDs. `/board-share HASH` approves that
preview for this scope and Codex; it includes at most 64 identities, never
future tasks automatically. It authorizes names/status metadata, not transcripts,
expert cards, project writes, or task control.

When asked what it can see or remembers, the assistant checks this scope's
current sharing state and the existing board projection. With sharing off it
cannot list project threads; with an unavailable, partial, or stale projection
it must say so. Approved rows are a bounded selection, not a complete fleet
inventory. Memory in a model turn is likewise a bounded retrieval, and a
model statement alone is not proof that a new preference or action was saved.
When standing guidance actually commits, the answer carries a separate native
save receipt; hypothetical wording stays a proposal rather than active guidance.
This self-check runs only for relevant questions and creates no provider scan.
Its short Pika-only workflow skill is bundled into that turn, not installed in
the user's Codex or Claude skill directories. The skill helps the model interpret
current native evidence; it cannot grant access or substitute for a receipt.
Consolidation and Reflection have their own bundled Pika-only skills; the
existing maintenance planner selects exactly one by purpose when work is due.

Private consultation has a separate permission: `/consult-allow PROVIDER:EXACT_UUID`
permits one exact local watched conversation until revoked. Inspect `/consults`;
use `/consult-revoke ID` or `/consult-forget ID` to remove future access. Provider
support and private-side isolation are checked before dispatch. A consultation
can disclose its result to Pika's model and consume allowance; it does not type
into or direct the original project agent.

## Optional background lifetime and recovery

After foreground enablement and board-sharing approval, `/background CALLS`
allows bounded background reasoning in the same scope, within the existing
allowance, until disabled. An optional HOURS argument requests a timed grant;
omitting it (or zero) does not require daily renewal. Existing timed grants are
not extended automatically. Each investigation reserves four calls before
dispatch and has a 120-second deadline; fewer than four remaining calls prevents
another dispatch. Verified completion can settle the actual bounded charge;
unknown delivery is not refunded or automatically retried.

The same host can then outlive its views. This does **not** install a login item,
system service, reboot launcher, notification channel, or continuously thinking
model. `/background off` disables that lifetime; `/pause` stops assistant
reasoning and owned work. `/resume` respects existing grants, expiry, and blocked
recovery. None of these commands stops project agents.

Fresh material attention episodes are coalesced; names and polling timestamps
do not cause paid work. Local canonical lifecycle events distinguish repeated
questions. Remote rows without such event identities provide observed status
transitions only, not a guarantee that every intervening event was seen.

`/cancel` cancels owned work. `/fresh-context` explains recovery;
`/fresh-context acknowledge` explicitly retires the owned provider context
without replay or refund. Saved Pika memory remains; re-enable the provider
explicitly afterward. Unknown delivery/cleanup remains visible.

`/board-share off` revokes sharing, conservatively invalidates worker-derived
findings in that scope, and blocks reuse of the disclosed provider context until
recovery completes.
`/forget ID` invalidates dependent Pika memory and derived caches/queued reuse.
Neither promises deletion from provider logs, external copies, or backups.

## Tool workshop

`/evolve CORRECTION_ID` saves a scoped improvement proposal without a model call
or activation. Bare `/evolve` gives guidance; it does not run a demonstration.
To authorize authoring, `/evolve-json JSON` takes `need`, two to eight protected
`cases`, and optional `correction_id`. Each case has `inputs` and `expected`;
each input is `{"value":DATA,"scope":{"values":["project-a"]}}`, matching the
enabled scope exactly. The author does not receive the protected test cases.
Authoring uses the existing paid allowance; evaluation runs in the restricted
native data interpreter.

Routine learning is part of conversation, not a workshop task for the user.
The JSON commands below remain advanced developer controls for executable
experiments; they are not prerequisites for persona or working-approach learning.

Inspect `/tools` and `/tool-info HASH` for the immutable definition, test/cost
evidence, grants, assessments, and rollback history. `/approve HASH` activates
that eligible exact version without time expiry. `/tool NAME JSON_ARRAY` invokes it on
the supplied scoped values, without a model call. Restart does not erase its
catalog or approval. Revocation, retirement and source invalidation still disable
use; approving one version never approves a later version. Rollback and method
approvals also have no time expiry. Previously issued expiring grants keep their
original terms until explicitly re-approved; no revoked or retired tool is revived.
In the interactive view, `/approve` approves the passed version currently displayed;
the client sends that exact version, not whichever version happens to be latest later.

`/assess-tool JSON` records human evidence with `hash`, `outcome`, `evidence`, and
optional `rollback` hash. Outcomes are `helped`, `neutral`, `regression`, or
`retire`. Regression/retirement disables the assessed version; a requested
rollback must independently qualify. `/rollback NAME HASH` selects an exact
eligible version; `/revoke GRANT_ID` revokes its grant. Tool creation, testing,
activation, later assessment, and rollback are distinct events.

Generated tools transform supplied data only. They cannot use ambient files,
shells, networks, credentials, installs, or processes. Size, memory, operation,
depth, cancellation, and execution-time limits are enforced outside the tool.
No candidate can edit its grants, protected tests, or recorded failures.

## Continuity and maintenance

Ordinary Luna turns can return an answer and bounded source-linked learning in
one call. Human wording stays distinct from the model's interpretation. A
proposed decision is not acceptance or execution. Unsupported/ambiguous lasting
preferences remain proposals; supported low-risk guidance can cover persona,
collaboration and working approach in plain English, with an optional condition.
There is no four-category limit or exact-phrase requirement. Such interpretation
is fallible, remains attributed to the model and never becomes human authority.
Current explicit instructions win conflicts. Guidance is
reversible with `/guidance-off ID` and `/guidance-on ID` in its exact scope.
If a learning save fails, the answer remains available with a save warning.

Learned commitments start as source-linked proposals. Existing `/decision-state`
controls record acceptance or deferral without executing anything.
`/commitment-due ID UNIX_SECONDS` records a due condition for native briefs and
fresh context independently of Reflection; it schedules no model call or action.
`/commitment-done ID STATEMENT` records an attributed human completion report,
not an execution receipt. Only applicable native receipt evidence can confirm
execution. Forgetting the source removes dependent commitment history.

After foreground enablement, `/maintenance CALLS` opts in to daily memory review
until disabled, within the existing total allowance. Optional
`/maintenance CALLS HOURS INTERVAL_HOURS` sets a time limit and cadence. For example,
`/maintenance 2 24 12` allows at most two background calls during the next
24 hours, with a Reflection opportunity every 12 hours. The interval is elapsed
time, not a timezone-dependent appointment. A tick with no meaningful eligible
evidence spends nothing. `/maintenance` shows status and coverage gaps;
`/maintenance off` disables it. `/pause`, expiry and exhausted allowance apply.
This is memory-only permission, not permission to read transcripts or board rows.

Exact observed compaction events signal consolidation without calling a model
or delaying compaction for an essay. Missing/unsupported events do not imply
complete coverage: bounded catch-up uses already permitted durable memory.
Consolidation and Reflection have separate prompts and coverage. Each job uses
one disposable Luna turn, at most 32 KiB/64 references of input, 8 KiB output,
and 120 seconds (or the tighter grant deadline). No automatic repair/synthesis
call follows. Unknown delivery remains charged and is never blindly retried.

`/revisit ID HOURS` records an explicit due reconsideration of older evidence;
it does not increase permission or budget. Oversized sources stay available in
the memory browser and are reported as deferred; they cannot starve smaller work.
Findings, covered revisions, and pending workshop handoffs commit together.
Confirmed receipts can finish local bookkeeping after restart without another call.

`/proposals` lists pending method/tool improvements. Inspect a proposal with
`/memory-record ID`. `/method-test-json JSON` accepts its `proposal_id`, an
`experiment`, immutable `candidate`, and three distinct case groups:
`original_failure`, `contrasting`, and `protected`. Cases use the existing
workshop `inputs`/`expected` structure. Method input is exactly
`{"queries":["current question"]}`; method output is an array of bounded
plain-English working steps, not executable commands or permission changes.
Legacy step names remain readable. Testing is native and does not activate the
candidate. Inspect `/tool-info HASH` before `/method-approve HASH` (methods) or
`/approve HASH` (tools). `/method-assess-json JSON` records later evidence and
optional eligible rollback. Approved methods and bounded tool references are
loaded in fresh contexts; invalid or forgotten sources block their reuse.

All of this runs in Rust in the existing authority host. Views consume cached
status; slow maintenance preparation and native evaluations run off the view loop.
Native workshop results appear in the assistant's cached status when complete.
Owned maintenance and evaluation workers stop before forgetting, board-source
revocation, or explicit fresh-context recovery. Forgetting
scrubs Pika-owned maintenance journals as well as derived memory; provider-side
logs, external copies and backups remain outside that guarantee.

## Pika's counterparts to Muse's standing Markdown files

Pika borrows the *jobs*, not a second set of editable authorities. Its bundled
`pika-skills/*/SKILL.md` files cover self-awareness, consolidation, Reflection,
and user-invoked feedback. They are selected for a specific turn, typed
maintenance job, or explicit feedback command. Other Muse-style files map to
Pika's existing native state:

`/profile` lists three read-only virtual Markdown views. `/profile identity`
shows `IDENTITY.md`, `/profile soul` shows `SOUL.md` (built-in orientation and
currently applicable learned guidance), and `/profile memory` shows `MEMORY.md`
(up to 16 selected active records). They are generated on request, make no
provider call, and are not files to edit. `/memory` remains the paged scoped
archive; `/memory-record ID` shows full evidence. The views make their coverage
limits explicit.

| Muse file | Pika owner |
| --- | --- |
| `IDENTITY.md` | The immutable assistant profile ID and its user-facing name. |
| `SOUL.md` | Protected purpose plus inspectable, scoped, reversible guidance and evaluated methods; no model-editable master persona file. |
| `USER.md` / `MEMORY.md` | Attributed, versioned, scoped memory in SQLite, including corrections and forgetting. |
| `AGENTS.md` / `TOOLS.md` | Evaluated working methods and the separately approved tool registry, not project-repo `AGENTS.md`. |
| `HEARTBEAT.md` | The enabled maintenance planner, explicit commitments, and grant limits; an empty or unchanged opportunity is quiet. |
| `PROACTIVE_PREFERENCES.md` | Briefing/attention policy; no new notification channel or automatic outreach is implied. |

The three views above are projections; the remaining rows are conceptual
counterparts, not profile files Pika reads or writes. Copying Muse's files into
the profile would create stale, conflicting truth. Pika does not import Muse's
personal memory, schedules, or tool access automatically.

## Operator and deployment boundaries

Assistant state is under Pika's state directory in `assistant/` (normally
`~/.local/state/pika/assistant`; `PIKA_STATE_HOME`/`XDG_STATE_HOME` can change it).
Private SQLite stores separate memory, policy/accounting, ownership, runtime,
learning, and workshop state. Preserve the whole profile when retaining state;
do not delete ledgers to reset spending or move/replace a live profile. Private
file permissions are not encryption or protection from arbitrary same-user code.
Set an explicit backup/deletion policy before live enablement.

Board cues read dated local caches without starting an assistant or model.
Missing, stale, partial, and unavailable coverage must not be read as “nothing
needs attention.” Compatible views share an observation lease; do not assume
older installed boards participate. Close incompatible old views before a
cutover rather than restarting project agents.

Windows is an explicitly bound client to one existing macOS/Linux authority;
see [assistant client](ASSISTANT_CLIENT.md). There is no offline shadow assistant,
automatic authority failover, native Windows provider host, or iOS client here.
Project writes/delegation, arbitrary artifact reads, fleet rollout, and broader
notification channels are not implied by enabling the assistant.

Hermetic tests demonstrate safety and behavior against synthetic state/fake
providers; they do not establish live usefulness, actual model cost, or native
Windows Terminal/SSH compatibility. Live trials, installation, migration,
publication, and rollback retirement remain separate release-owner decisions.
Follow [release format](RELEASE_FORMAT.md) and [releasing](releasing.md) for
release gates; this guide does not authorize deployment.
