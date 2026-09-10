# Pika video

[Watch Pika in action](../../README.md#pika).

The film starts with the board and returning to a conversation by name, then
shows an agent discovering prior expertise and consulting it while the original
agent keeps working. It closes with trusted machines and setup: use the board
first, then add expert cards for consultations. The projects and interfaces are
illustrative. No live conversations or credentials are used to render it.

## Build the earlier experience tour

The earlier tour uses [experience-tour.html](experience-tour.html) and
[experience-tour.js](experience-tour.js). It uses local HTML/CSS and deterministic
animation. Rendering requires Node.js, Chrome, Playwright and FFmpeg:

```bash
cd media/launch
npm install
CHROME_PATH='/path/to/chrome' node check_experience_tour.cjs
CHROME_PATH='/path/to/chrome' FFMPEG_PATH='/path/to/ffmpeg' \
  node render.cjs --page experience-tour.html --out ../../dist/new-tour
```

The renderer produces a 1920×1080, 30 fps, silent MP4 and representative frames.
Use a fresh output directory; it does not overwrite an existing movie. Add
`--stills` to export frames without encoding the video.

Open the HTML file for an interactive preview: Space pauses, arrow keys seek,
and `?t=19` starts at a specific time. Earlier film sources remain available in
this directory. Browser and encoder packages are video-development dependencies,
not Pika runtime requirements.

The `proof/` directory contains a separate synthetic retry fixture used in an
earlier demo. Run `python3 media/launch/check_evidence.py` from the repository
root to verify it. Its results describe that fixture, not production performance.
