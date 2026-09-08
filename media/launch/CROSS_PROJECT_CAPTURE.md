# Cross-project capture: discovery worked; consultation did not

This is a development record, not launch copy or a successful-consultation claim.
Two native Codex conversations built separate, deliberately designed example
projects against an invented localhost Activity service. No work projects, real
credentials, real customer data or production endpoints were used.

## Observed sequence

1. The reporting agent built a weekly CSV exporter, its auth client and tests.
   Its project instructions included the fixture's deployment-specific gateway
   audience. Eight tests passed against the service. This was an integration
   exercise, not an independently discovered production incident.
2. One Pika interview populated that conversation's expert card in isolated
   inventory. It did not add the project to the user's normal Pika board.
3. A separate dashboard agent received a weekly-view task and a client reference,
   with the agent-convo skill available. It was not given the reporter's name,
   native conversation ID, or working token audience. Fixture source and other
   project setup notes were outside its instructed project scope.
4. It read the reference and exercised the endpoint. Token issuance returned 200;
   the weekly data request returned 403. It then searched Pika for
   `Activity gateway token audience weekly activity`.
5. The discovery result identified the reporting conversation. Importantly, the
   expert card's current-work text already included the working audience.
6. The agent attempted `pika ask` for the exact discovered ID. Preparation failed:
   the nested workspace sandbox prevented the provider from initializing its
   native SQLite state. Receipt: **not_sent, cleanup complete, zero answers**.
   There was no delivered question or successful private exchange.
7. The agent used the card's audience as a hypothesis and checked it against the
   service. The data request returned 200. It built the dashboard and its tests,
   and explicitly reported the failed consultation rather than implying success.
8. The capture runner independently reran all six dashboard tests, including the
   live service test, and regenerated the HTML: three weeks, 293 synthetic events.

## What this supports

Natural cross-project discovery: ordinary task progress, a specific obstacle,
relevant metadata, and verification back in the original project. The reporting
parent's transcript SHA-256 and complete project-file manifest matched before and
after the dashboard run. Those hashes establish preservation for this run only;
they do not prove successful consultation, which did not occur.

## What it does not support

- A successful agent-to-agent answer or a question-to-answer latency claim.
- A claim that Pika caused the dashboard success through private consultation.
- A ranking-quality benchmark: there was one candidate in the isolated inventory.
- A claim that the card's contents were unknown to the asking agent until an
  expert replied. The working setting was visible in metadata.
- General authentication advice, production security, or a real customer story.

The current film script remains a proposed successful journey, not a transcript
of this run. Do not use the previous refund answer or a separately conducted ask
to conceal this failed preparation stage.

## Implications for the next capture

First establish an approved execution path for native provider state access;
do not silently weaken the demo sandbox or bypass Pika after an error. Then use
a meaningful question whose answer is not already available in the card. A
card should help select an expert; sometimes it can also resolve the gap outright.
That is a valid product outcome, not a reason to force another model call for
the film. Do not hand-edit a card to hide useful information for dramatic effect.

Raw prompts, tool events, receipts, source, tests and preservation fingerprints
remain local in the ignored `dist/cross-project-demo/` capture directory. This
summary does not make those private native records publicly reproducible.
No new successful-consultation video was rendered, committed, pushed or published.

## Permission-approved second attempt

The user approved native Codex session-state writes for the demonstration. A
fresh dashboard run retained the workspace sandbox and added Codex's home as an
invocation-scoped writable directory; no saved permission settings were edited.
The original reporting project and its unedited expert card were reused.

The new agent again tested the API, observed 403, and discovered the reporter.
It read the working audience from metadata, verified it against the service, and
explicitly decided no consultation was needed. It completed a reusable data client
and dashboard. All seven tests passed on independent rerun. The expert transcript,
expert project manifest, and provider configuration hashes remained unchanged.

No ask was attempted in this second run, so it neither proves the consultation
permission path works nor supplies a successful exchange for the film. It does
reinforce that this example's gap is answered by the card. Do not force or stage
an unnecessary consultation, hide useful card content, or spend further calls
repeating the same setup to obtain a preferred narrative.
