// Verbatim answer from a real Pika side consultation, not authored film dialogue.
// Synthetic project only. Identifiers and raw transcripts remain outside public assets.
// The displayed question omits only brevity/no-tools instructions; see STORYBOARD.md.
window.PIKA_CONSULTATION = {
  question: "Why did we make this endpoint read-only?",
  answer: "Support staff only need to inspect sample orders. The separate billing service owns all order changes and their audit trail, so allowing dashboard mutations would duplicate responsibility and risk bypassing billing’s controls. The endpoint therefore permits only GET, HEAD, and OPTIONS.",
  provider: "Codex",
  route: "local",
  parentState: "parked",
  proof: {
    parentTranscriptUnchanged: true,
    projectFilesUnchanged: true,
    cleanupConfirmed: true,
    scope: "One recorded consultation; external before/after measurements, not a universal guarantee."
  }
};
