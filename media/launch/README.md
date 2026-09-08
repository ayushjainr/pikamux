# Pika launch film — promise-and-proof preview

Code-rendered, captions-only, 1920×1080 at 30 fps. No generated video, stock
footage, provider logos, external fonts, or music. Nothing is uploaded by these
scripts. Review the MP4 before sharing.

The board frames come from Pika's actual `render_monitor` function with synthetic
sessions. They are labelled **Real renderer · synthetic projects**. This is a
product walkthrough, not a continuous live screen recording. The consultation
segment preserves the answer's words from the Codex conversation that built
and tested a deterministic synthetic refund-retry experiment. The answer was obtained
through `pika ask`, not scripted; the private DESIGN.md link path is removed.
The target came from a real `pika experts refund --json` result following an
explicit expert interview. The card shows actual selected metadata in a designed
view, not a new TUI component. The user task is illustrated; the film-making agent
performed the recorded discovery and consultation. It is a **local** example; the cross-server
claim is backed by separate recorded Codex and Claude tests. The accompanying
results are scoped to those tests, not blanket guarantees. No real names, UUIDs,
server identifiers, credentials, transcript files, or private project paths are
included in the public assets. No agents are contacted by the renderer.

## Rebuild

```bash
uv run python media/launch/export_board.py
cd media/launch
npm install
# Point CHROME_PATH at an installed Chrome/Chromium executable.
# Point FFMPEG_PATH at an ffmpeg build with libx264.
node render.cjs --out ../../dist/launch-film
node check.cjs
```

`index.html` is an interactive preview: Space pauses, arrows seek five seconds,
and the bottom slider scrubs. `?t=34` starts paused at a specific second.
`window.renderFrame(seconds)` gives deterministic frame-by-frame rendering.

`render.cjs --stills` exports representative PNGs without encoding the film.
Browser and encoder dependencies are production tools only, not Pika dependencies.
Rendered MP4/PNG files stay under ignored `dist/`. The source is self-contained
and can be revised without re-recording a desktop or changing Pika itself.

The new render is named `pika-promise-and-proof.mp4`; earlier cuts are retained
separately. The first ten seconds show task → expert card → expert reply. The
main story is agent discovery and consultation, with the board as a short human
view into the same network. The retry-button proposal is checked against the
expert's design note but not implemented. See STORYBOARD.md for claim boundaries.

## Inspect the proof before calling a model

The designed fixture measures two refunds after a fresh-ID retry versus one when
retaining the original ID. Its source, tests, design note and sanitized result
ledger are in [proof/](proof/). Reproduce the fixture offline:

```bash
python3 media/launch/check_evidence.py
```

This checks the source hashes, actual counts and four tests—not the private native
consultation receipts. The [storyboard](STORYBOARD.md) distinguishes those evidence
layers. It is not a real payment incident or a benchmark of AI quality.

The [launch playbook](../../docs/launch-playbook.md) supplies two openings, draft
copy, a bounded first-use invitation and an opt-in manual audience test. It does
not authorize posting or collect telemetry. **Promotion is gated on a verified
release containing the compatibility fix.** This preview is visibly labelled.

## Installation text for the description

The film links to the repository for instructions rather than asking viewers to
transcribe a long command. This pinned public-alpha command remains copyable here:

```bash
# macOS or Linux — review the script before running it.
curl -fsSL https://github.com/ayushjainr/pikamux/releases/download/v0.5.0a3/install.sh | bash -s -- --version v0.5.0a3
```

Native Windows hosting is not supported. Accept setup to include the agent-convo
skill. The recorded consultation used the local source with the paginated-parent
Codex compatibility fix; that fix must ship in a release before presenting this
as the behavior of the pinned public package on affected Codex sessions.
