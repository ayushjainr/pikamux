# Promise-and-proof cut: an expert network for your agents

72 seconds. Captions only. HTML/CSS/browser rendering, no video model.
Development preview: local Codex compatibility fix 6b8af0f is not in public a3.
The main promise is agent-discovered expertise and agent-to-agent consultation;
the board is a supporting human view, not the protagonist.

## First ten seconds

- 0–3s: Illustrated user task: “Add a refund retry button.”
- 3–6s: The current agent's search reveals a real expert-card excerpt.
- 6–10s: The expert's actual first sentence reports two refunds versus one.
- By 8s the payoff is fully visible: “Your agent found someone who already knew.”

The capture-making agent performed the real discovery and consultation. The user
task and proposed application are illustrated, not an autonomous implementation.
Designed views compress timing; this is not a continuous screen recording.

| Time | Beat | Evidence |
| --- | --- | --- |
| 0–10s | Task → card → reply | Actual selected card and exact answer excerpt |
| 10–16s | Your agents: an expert network | Skill + discovery + private consultation |
| 16–28s | A card gives a reason to ask | Real selected metadata in a designed view |
| 28–40s | Current agent asks the discovered expert | Recorded answer; only private link path removed |
| 40–49s | Context informs the proposed approach | Stable operation ID and same payload; checked against design record |
| 49–57s | One intended refund, two observed outcomes | Reproducible fixture; scoped preservation/cleanup evidence |
| 57–64s | Your board into that network | Real renderer with synthetic inventory |
| 64–72s | One project, one card, one useful consultation | First-use invitation, visible preview/platform caveats |

## Measured experiment — inspect it yourself

A new persistent Codex builder implemented and ran a deterministic synthetic
refund-retry fixture. Its specified upstream commits before losing a response,
then deduplicates solely by operation ID. One intended refund, one lost response,
one retry. Fresh retry ID: two refunds. Original ID retained: one refund.
The agent's four unittest tests passed; the capture-making agent reran them and
the experiment independently before relying on the result.

[experiment.py](proof/experiment.py), [tests](proof/test_experiment.py),
[DESIGN.md](proof/DESIGN.md), and [sanitized evidence ledger](proof/evidence.json)
are included. To reproduce without Pika, authentication, network or model calls:

~~~bash
python3 media/launch/check_evidence.py
~~~

This verifies source hashes, reruns the counts, and runs the tests. It does not
independently reconstruct the native consultation from public assets: raw native
receipts and transcript fingerprints remain private. Their recorded outcome is
separately audited, not something source hashes or literal booleans alone prove.

The fixture is a deliberately designed experiment, not a production incident,
money saved, a ranking benchmark, or proof that a fresh model would fail. It
omits persistence, concurrency, expiry, validation and payload-conflict handling.
It is not a payment-ready implementation or universal retry advice.

## Discovery and recalled answer

One explicit Pika expert interview populated the builder's card in isolated demo
state. Then pika experts refund --json found refund-retries with topic, scope,
and name matches. Search used metadata without a model call. Card creation and
consultation used provider quota. One candidate proves the discovery path, not
ranking quality at scale. PARKED is an actual captured state, not fake activity.

discovery.js contains an exact scope excerpt, three actual topics, artifact
basenames, actual match fields and source availability. It is a designed view,
not an existing TUI component. An expert card is a relevance aid, not a verified
authority certificate. Durable expertise is distinct from current activity.

The next question used the exact native ID from discovery and did not supply
the retry outcome or prescribed solution. The displayed question omits only:
“State the measured counts and cite the artifact. Use at most 50 words of plain
prose. No tools or file changes.” The answer recalled two versus one, recommended
retaining the ID, and cited DESIGN.md. Its words are preserved, with the private
Markdown link target removed. The first sentence is the cold-open quote.

The proposed application was checked against DESIGN.md and the experiment.
The film does not claim that a retry button was implemented or a mistake was
prevented. A real implementation still needs its provider's actual contract.

## Isolation, builds and publication

The recorded Pika consultation completed one turn on the default profile, then
confirmed cleanup complete and discarded true. External capture scripts compared
parent transcript SHA-256 and complete demo-project file manifests before/after;
both matched. The original was parked. Ordinary Pika receipts do not perform
that external audit. No simultaneous coding or universal preservation is claimed.

The first synthetic builder, the explicit card interview, and the consultation
were the three model-consuming operations; display and metadata search made none.
No work projects or real payment systems were used. Pika's ordinary user inventory
was not populated with these demo targets.

This is a development preview, labelled throughout. Release and publication gates
are in the [launch playbook](../../docs/launch-playbook.md). Older releases and
film cuts remain unchanged. Separate earlier Codex/Claude cross-server tests support
only the closing capability note; this capture is local. OpenCode was not exercised
here. Native Windows hosting remains unsupported.

## Why both lenses

Rory-inspired framing: relieve the human of being the intermediary; a parked
conversation can still be a valuable expert. Hopkins-inspired execution: a
specific promise, an observable result with reproducible evidence, a small trial,
and an honest test of whether users understand and succeed.

The strongest three changes are the early expert reply, measured two-versus-one
result, and the first-consultation path that accounts for missing cards. Audience
comprehension and useful activation remain hypotheses, not measured improvements.
Use the consent-based test plan before claiming reach, retention or conversion.
