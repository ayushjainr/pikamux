# Pika — experience across projects

60-second illustrative film, authored in HTML/CSS/JavaScript. Captions only,
1920 × 1080, 30 fps. No video-generation model or live provider calls are used
to render it.

## Story

An agent starts building a weekly dashboard view, encounters a data-access
problem, and discovers that a separate reporting project uses the same service.
It asks the conversation behind that expert card about the setup, checks the
answer, and continues building. The user does not name an expert or request a
consultation, and the agent is not shown checking with others before starting.

| Time | Scene |
| --- | --- |
| 0–6s | The dashboard agent starts its task and connects the service. |
| 6–12s | Data access fails; the reporting expert card appears by 8s. |
| 12–20s | Pika discovery connects the two projects through their shared service. |
| 20–31s | The dashboard agent asks one focused question. |
| 31–43s | The reporting agent explains its setup and points to artifacts. |
| 43–52s | The dashboard agent verifies the connection and returns to the view. |
| 52–60s | Product description, repository, and personal website. |

## Scope

The user explicitly approved a synthetic story. Dialogue, expert-card contents,
statuses, filenames and interface are illustrative—not quotations from a live
consultation or proof that a particular agent autonomously completed this flow.
The whole film is labelled “Illustrative demo · Development preview.” The gateway
configuration is invented for the story, not general authentication advice.
The earlier [live attempts](CROSS_PROJECT_CAPTURE.md) remain separately documented;
their results are not represented as a successful consultation in this film.

This preview does not change Pika's released capabilities. Public v0.5.0a3 lacks
the local paginated-Codex compatibility fix; the README retains that release gap.
No launch, package release or public upload follows merely from rendering.

## Rebuild

From the repository root, with the launch renderer dependencies installed:

```sh
CHROME_PATH='/path/to/chrome' node media/launch/check_cross_project.cjs
CHROME_PATH='/path/to/chrome' FFMPEG_PATH='/path/to/ffmpeg' \
  node media/launch/render.cjs --page cross-project.html --out dist/new-cross-project-cut
```

`cross-project.html` is an interactive preview: Space pauses, arrows seek,
and the slider scrubs. `?t=36` opens at a paused frame; `?render=1` hides controls.
`cross-project.js` holds the fictional story copy. The renderer refuses to
overwrite an existing MP4. Older cuts remain untouched.

The repository README embeds the animated version at `media/demo/pika.gif` and
links to the full-resolution H.264 MP4 at `media/demo/pika.mp4`. This avoids relying
on a standard HTML video element, which the GitHub repository-context Markdown
rendering check stripped. An inline GIF remains visible without a separate video
host, an attachment issue, or an installation. GitHub documents supported image
formats in [Working with non-code files](https://docs.github.com/en/repositories/working-with-files/using-files/working-with-non-code-files).
