// Real Pika consultation selected using the recorded discovery result.
// Words preserved; the DESIGN.md Markdown link's private path is removed.
// Displayed question omits only citation, brevity and no-tools instructions.
window.PIKA_CONSULTATION = {
  "question": "I'm planning to let support staff edit orders in this dashboard. Which earlier decision should constrain my approach?",
  "answer": "The dashboard was deliberately read-only because billing owns mutations and their audit trail. Any edit must delegate to billing with authorization, validation, and defined failure and retry behavior; merely enabling POST or changing local data violates that boundary. See DESIGN.md.",
  "provider": "Codex",
  "route": "local",
  "parentState": "parked",
  "proof": {
    "parentTranscriptUnchanged": true,
    "projectFilesUnchanged": true,
    "cleanupConfirmed": true,
    "scope": "One recorded consultation; external before/after measurements, not a universal guarantee."
  }
};
