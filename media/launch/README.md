# Pika launch film — first cut

Code-rendered, captions-only, 1920×1080 at 30 fps. No generated video, stock
footage, provider logos, external fonts, or music. Nothing is uploaded by these
scripts. Review the MP4 before sharing.

The board frames come from Pika's actual `render_monitor` function with synthetic
sessions. They are labelled **Real renderer · synthetic projects**. This is a
product walkthrough, not a continuous live screen recording. The consultation
segment uses an exact, non-sensitive excerpt from a previously recorded Pika
cross-server test; its question is explicitly abbreviated. The accompanying
results are scoped to that test, not blanket guarantees. No real names, UUIDs,
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
