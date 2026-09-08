// Actual answer words; only the private DESIGN.md Markdown link target is removed.
// Display is designed; full capture/protocol details are in STORYBOARD.md.
window.PIKA_CONSULTATION = {
  "question": "I'm adding a refund retry button. What did your earlier experiment find that should constrain my implementation?",
  "answer": "The synthetic experiment measured 2 refunds with a fresh retry ID versus 1 with the original ID. Assign one operation ID per intended refund and reuse it after timeouts; a timeout may follow a successful commit. See DESIGN.md.",
  "provider": "Codex",
  "route": "local",
  "parentState": "parked",
  "proof": {
    "parentTranscriptUnchanged": true,
    "projectFilesUnchanged": true,
    "cleanupConfirmed": true,
    "scope": "One synthetic local capture, externally measured. Not a universal guarantee."
  }
};
