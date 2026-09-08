# Cross-project consultation — script draft

Status: original narrative draft. The user subsequently approved a synthetic
story, now rendered in [the cross-project film](CROSS_PROJECT_FILM.md). Its final
dialogue is in `cross-project.js`; the timings below are the initial proposal.
The older refund cut is preserved separately, not used for the README preview.

First live capture: the dashboard agent encountered 403, discovered the reporting
project itself, and verified a working audience from the expert card. Its Pika
consultation failed before delivery because its workspace sandbox did not permit
Codex's native state initialization. No expert answer was received. That run
supports natural discovery, not the successful consultation pictured below.
Do not splice in an answer from a different run and imply uninterrupted success.

## The story

A dashboard agent is connecting an internal data service. A separate conversation
previously built a weekly report exporter using the same service. The dashboard
agent encounters an authentication problem, discovers that relevant experience,
and asks about the integration. It checks the answer and continues its own task.

The user requested a dashboard, not an expert search. Neither project depends on
the other. They happen to use the same service.

The dialogue and filenames below are illustrative. Do not present them as actual
agent output, a customer incident, or a measured result. The service-specific
authentication behavior is invented for this draft, not general technical advice.

## Screen script — approximately 60 seconds

Captions only. Keep the two project names visible during discovery and the
consultation. Show one exchange, not a stream of agents checking with one another.
Dialogue is ordinary visible output, not a depiction of hidden reasoning.

### 0–5s — Work already underway

Project label: **Operations dashboard**

User request:

> Add a weekly activity view to the dashboard.

Brief work progression: page layout, data connection, first request. These are
shots of work, not three more explanatory captions.

### 5–10s — The overlap

Connection result:

> Sign-in succeeded. Data request: 403.

Transition to a Pika discovery result, still within the dashboard agent's workflow:

**Weekly report exporter**

> Uses the same data service · authentication setup

By ten seconds, the viewer should understand both the obstacle and why this
different project might help. Do not force a complete answer into the opening.

### 10–18s — A reason to ask

Small label: **Pika · expert card**

Show the selected card, using only the relevant portion:

> Weekly report exporter
>
> Built the scheduled report export. Integrated the data service,
> including its token setup and access checks.

The card is a summary of relevant prior work, not a certificate of correctness.
Do not show the dashboard agent asking an arbitrary peer for permission to proceed.

### 18–28s — One specific question

Label: **Dashboard agent → Report exporter agent**

> I'm connecting the dashboard to the same data service. Sign-in works,
> but data requests return 403. Did you need anything beyond the
> standard sign-in setup?

The discovery should follow ordinary local checks of the failed request; do not
stage the agent consulting someone for every error or before looking at its code.
Those checks need not become a tutorial in the film.

### 28–39s — Useful project history

Label: **Private consultation**

> Yes. Our gateway expects a separate audience for the data API.
> The sign-in token wasn't enough. The setup is in auth_client.py;
> test_data_access.py checks it.

Show the artifact references as references, not invented clickable product UI.
No credentials or token values appear. This is advice from a prior integration,
not authorization to change the dashboard's authentication blindly.

### 39–49s — Back to the dashboard

Return to the original project. Show the agent checking the referenced setup
against its own connection, applying the relevant change, and running its test.

Result:

> Data connection passed.

Then show the weekly activity view loading. The consultation is over; the agent
is building the dashboard again. Do not replace this outcome with a feature list.

### 49–55s — Explain only what the scene has earned

> Pika lets your agent find relevant work in your other projects
> and ask the conversation that did it.

Keep the two project labels as a quiet visual callback. No committee graphic,
glowing network, intelligence score, or claim that every task needs consultation.

### 55–60s — Close

**Pika**

> Expert discovery and private consultation for coding agents.

Repository: **github.com/ayushjainr/pikamux**

Keep applicable preview and platform caveats legible. Do not squeeze a board
tour, installation tutorial, or provider matrix into this ending.

## Recording requirements

- Prefer a genuine, permission-cleared cross-project episode matching this shape.
  Do not inspect or publish private work-project material just to fill the script.
- If we use a designed demonstration instead, label it throughout. Two actual
  project contexts must exist before discovery; do not feed the answer to the
  asking agent or retrofit a fake card around it.
- Capture ordinary task progress, the actual obstacle and prior local checks,
  metadata discovery, the selected conversation, the bounded question and answer,
  and verification in the asking project. Preserve actual wording where quoted.
- Use captured metadata for the expert card. A designed view must be identified
  as such; this script does not specify a new Pika UI feature.
- Do not imply that Pika transfers files or secrets. Any artifact inspection needs
  an actually supported, authorized path; remote references may not be local files.
- Verify the shown result. If the answer does not fix the connection, show that
  honestly or choose another episode; do not manufacture a passing test.
- Keep parent preservation and consultation cleanup as evidence requirements.
  Do not turn them into an unsupported claim that the other agent was coding
  simultaneously. Do not reuse the old refund capture as proof of this story.
- Metadata lookup does not require a model call. Interviews and consultation do.
  Show no periodic polling, automatic panel, or obligatory consultation per task.
- Timing is a proposed edit, not a speed claim. Adjust the cut to readable captured
  material. Keep “illustrative scenario” visible on any preview using this draft.

## Editorial check

The viewer should be able to answer:

1. What was the dashboard agent trying to build?
2. What specific problem made the other project's experience relevant?
3. How did it find that conversation without the user supplying its name?
4. What did the answer let it do next?

If the takeaway is “my agent asks other agents before doing things,” the cut
has failed, regardless of technical correctness or internal review scores.

Avoid “unlock,” “seamless,” “supercharge,” “collective intelligence,” and abstract
claims about agents becoming smarter. Let the work and the exchange carry the film.
