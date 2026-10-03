# iOS asset sources

Recorded 2026-10-03. The user explicitly replaced sparkle branding with a temporary Pikachu tail, then supplied a visual reference and rejected the earlier abstract silhouette and indigo palette. The current SVG recreates that reference's tall angled yellow tip, narrowing zigzag and brown base. It is not an official Pokémon logo or endorsement. Provider assets remain unchanged; branding now uses yellow, charcoal and warm neutral surfaces. No image-generation model or user credential was used.

## Catalog names and integration

| Xcode name | Asset | Integration |
| --- | --- | --- |
| `AppIcon` | 1024×1024 opaque RGB PNG | Set the target app-icon catalog name to `AppIcon`; the OS applies its own corner mask. |
| `PikaMark` | 256×256 transparent PNG | Original full-color yellow/brown header and tab mark; inactive tab is fainter. |
| `ProviderOpenAI` | 256×256 transparent PNG | Template image; OpenAI/Codex provider identity. |
| `ProviderClaude` | 256×256 transparent PNG | Template image; Claude provider identity. |
| `ProviderOpenCode` | 256×256 transparent PNG | Template image; OpenCode provider identity. |
| `ProviderMuse` | 256×256 transparent PNG | Template image; the preview's temporary italic `m` lettermark, not official Muse artwork. |

Swift `Image("...")` names are the names above. Provider marks use `template-rendering-intent: template`; PikaMark uses original colors. Accessible provider names belong in the presentation layer. The native tab bar uses a bounded 22-point original-color projection of the same PikaMark, 65% opacity when inactive. Selecting Pika plays one 450ms base-pivot flick using six cached actual UIImage frames; deselecting cancels it, and Reduce Motion selects a static image. There is no idle animation, custom navigation replacement or private UIKit traversal. The asset catalog manifests do not edit the target's build settings.

## Provenance

The user-approved source is `pika-ios-wireframes.html`, retained in the conversation's local visualization directory (`019ff5b2-b91d-74a2-8c51-b98ae7c7ac8c`). Its provider masks pin these exact sources:

- OpenAI: [OpenAI SVG, @lobehub/icons-static-svg 1.90.0](https://cdn.jsdelivr.net/npm/@lobehub/icons-static-svg@1.90.0/icons/openai.svg).
- Claude: [Claude SVG, @lobehub/icons-static-svg 1.90.0](https://cdn.jsdelivr.net/npm/@lobehub/icons-static-svg@1.90.0/icons/claude.svg).
- OpenCode: [OpenCode SVG, @lobehub/icons-static-svg 1.90.0](https://cdn.jsdelivr.net/npm/@lobehub/icons-static-svg@1.90.0/icons/opencode.svg).

Those path geometries were preserved. Only `currentColor` was fixed to black and relative outer dimensions were fixed to 256 pixels for rasterization; transparent masks are tinted natively by the app.

`PikaMark` is a code-native recreation of the user-supplied clipboard reference (`codex-clipboard-142b125b-36c9-47de-817e-f46466483199.png`): tall angled upper tip, narrowing zigzag and brown base, with a charcoal outline. It replaces every native iOS sparkle brand/header/assistant-tab symbol. `AppIcon` uses the same yellow `#ffbc16` and brown `#a96b35` tail on charcoal `#29251f`, without baked-in corners. Pokémon/Pikachu rights remain with their respective owners. This is a user-requested temporary symbol, not a permanent brand or clearance claim. Light surfaces are warm cream `#faf8f2`, dark surfaces warm charcoal `#191815`; bronze light tint and gold dark tint avoid yellow text on white. Filled buttons use charcoal text on yellow.

The preview explicitly labels Muse `Muse · temporary lettermark` and renders an italic Georgia `m`. `ProviderMuse` rasterizes that temporary lettermark using the installed system Georgia font. No font software is bundled and no official Muse logo is asserted. A verified official replacement remains a future asset decision, not a fabricated mark.

LobeHub artwork licensing is MIT. Exact notices remain in `THIRD_PARTY.md`; its historical Lucide notice is retained, but no Lucide sparkle artwork is shipped in the current Pika mark. Trademarks remain their respective owners' property; artwork licenses do not establish endorsement or brand-use approval.

## Reproduction and visual checks

Editable source-native vectors live in `Pika/Assets.xcassets/.sources/`. Raster outputs were produced mechanically with the existing bundled Sharp renderer; no renderer/dependency is added to the app. The Muse glyph depends on the installed system Georgia font. `render.cjs` recreates the PNG outputs using Sharp supplied through `NODE_PATH`.

The six-image contact sheet `.sources/preview.png` was inspected visually: all provider marks are recognizable, fit inside their bounds, and have clear transparent backgrounds. `AppIcon.png` metadata was checked as 1024×1024, 3-channel RGB, with `hasAlpha: false`. All five display marks are 256×256 with transparency. JSON manifests were parsed and their referenced PNG files checked. This is asset-level QA, not proof of app target integration, a successful Xcode build, installed-device appearance, App Store acceptance or an audit.

## Recorded hashes

Native color/flick validation: focused `colored-tail-tab-01.xcresult` passed 1/0 with no runtime warnings. Actual normal-app [inactive tab](/Users/ayushjain/pika-planning/ios/evidence/2026-10-03-native-validation/tail-tab-inactive-01.png) and [selected tab](/Users/ayushjain/pika-planning/ios/evidence/2026-10-03-native-validation/tail-tab-selected-01.png) retain yellow/brown. [Recorded native video](/Users/ayushjain/pika-planning/ios/evidence/2026-10-03-native-validation/tail-flick-native-01.mov) and [actual frame sheet](/Users/ayushjain/pika-planning/ios/evidence/2026-10-03-native-validation/tail-flick-actual-frames-01.png) verify rendered rotation between 38 and 40.5 seconds; frames are chronological at 50ms intervals, not a mocked animation. Reduce Motion's static path was source-reviewed, not separately exercised in this test. No assistant Open action or connection occurred.

Final narrow revision uses the user's golden reference `#ffbc16` consistently in tail/icon/buttons/dark tint. The header excludes transparent side padding and uses 6-point explicit spacing (about 7–8 points visible tail-to-text); the tab layout is unchanged. `gold-gap-build-01.log` passed, and the actual normal [final light screen](/Users/ayushjain/pika-planning/ios/evidence/2026-10-03-native-validation/golden-tail-tight-header-01.png) verifies the warmer color and tightened gap.

Native integration verified after the reference-shaped tail and warm-palette revision: Simulator build `warm-tail-build-02.log` passed in the retained disposable root `/tmp/pika-ios-transport.b4aP3S`. Actual normal non-fixture screens: [light](/Users/ayushjain/pika-planning/ios/evidence/2026-10-03-native-validation/reference-tail-light-01.png), [dark](/Users/ayushjain/pika-planning/ios/evidence/2026-10-03-native-validation/reference-tail-dark-01.png). Calculated contrast: charcoal/yellow button labels 9.04:1, light bronze/cream tint 7.95:1, dark gold/charcoal tint 10.53:1. Earlier `normal-tail-onboarding-03.png` and `tail-app-icon-03.png` are historical first-iteration images, not the current reference-shaped mark. No connections or saved-state reset occurred; the original light appearance was restored.

SHA-256 of the retained editable vector and produced PNG:

| Catalog name | Source SVG SHA-256 | PNG SHA-256 |
| --- | --- | --- |
| AppIcon | `a4ec55f40a8d9bc07d3789b4938d1f14048f62d03ad60bd9b47b02133e271681` | `5431cb3af14a2c10fe312740df5b57a630fd1cc2521978bab5a5738d57b58133` |
| PikaMark | `c3ef4e3994b981cef2eebbafe8d49059f7ce3dbc76ec5a0d96b7b94e17052a54` | `87f4d51150ac7fbdee97244dfd7bb7b90e2a6811c81c0117155f02588d6d73ff` |
| ProviderOpenAI | `27782eb48fe022dcc50b28a1e201542bdb1facb72866c3406ba8e7096da89374` | `cb0689a543385631d3685d9a08da776324dd836820ef3e513e2de48d61db628e` |
| ProviderClaude | `4b58589f5482d07efc57a0d2cf82e400abc97b37127cc0c2c06043fb07f04aed` | `4676dc143a7cb705c73137342616b34b654ea7c73037c0cebe5d5a8b4f7c48c2` |
| ProviderOpenCode | `6a4ded88af1ef099ba19cf4c6eea52531ec13f77182935aa31ef3c94fae02a4b` | `6fd1ea113c4e3e9595f7f4f278cdbc79cc56470450b2fe0bc47f75a2c54fd035` |
| ProviderMuse | `a6ad85b89d6ee46f33e9face7ab6c1e4fec0646c8c0692ab05dc57d5fe1f039e` | `961ed314a8d5f33e811237a6a0b4c05553daeaa4a93dd416ab2ae9694734e571` |
