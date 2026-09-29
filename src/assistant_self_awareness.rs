//! On-demand grounding for questions about Pika itself. This consumes existing
//! owner snapshots; it does not discover providers or grant access.
use crate::assistant_observation::Snapshot;

pub(crate) fn requested(question: &str) -> bool {
    let lower = question.to_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .take(256)
        .collect();
    let has = |word| words.contains(&word);
    let about_pika = has("you") || has("your") || has("pika") || has("pikachu");
    if !about_pika {
        return false;
    }
    let asks = [
        "what", "which", "how", "can", "could", "would", "will", "did", "do", "does", "are", "is",
        "tell", "show", "list",
    ]
    .iter()
    .any(|word| has(word));
    if !asks {
        return false;
    }
    [
        "thread",
        "threads",
        "conversation",
        "conversations",
        "board",
        "task",
        "tasks",
        "remember",
        "memory",
        "forget",
        "saved",
        "save",
        "access",
        "permission",
        "permissions",
        "capability",
        "capabilities",
        "tool",
        "tools",
        "skill",
        "skills",
        "see",
        "know",
        "done",
        "work",
        "working",
        "maintenance",
        "running",
    ]
    .iter()
    .any(|word| has(word))
        || lower.contains("what can you do")
        || lower.contains("what can pika do")
        || lower.contains("who are you")
        || lower.contains("who is pika")
        || lower.contains("what does pika do")
}

pub(crate) fn skill_for(question: &str) -> Option<&'static str> {
    requested(question).then_some(include_str!("../pika-skills/self-awareness/SKILL.md"))
}

pub(crate) fn prompt_state(shared: bool, sample: Option<&Snapshot>) -> String {
    let board = match (shared, sample) {
        (false, _) => "Board metadata sharing is disabled for this scope. Do not claim to see or list current project threads.".to_owned(),
        (true, None) => "Board metadata sharing is enabled for selected identities, but no projection is available now. Do not claim a current thread list.".to_owned(),
        (true, Some(snapshot)) => format!(
            "Board metadata sharing is enabled for selected identities only. The projection below was sampled at Unix time {} and is {}. It is not an exhaustive fleet inventory; stale rows are historical, not live proof.",
            snapshot.sampled_at,
            if snapshot.partial || snapshot.rows.iter().any(|row| row.stale) {
                "partial or stale"
            } else {
                "fresh but permission-bounded"
            }
        ),
    };
    format!("Current Pika self-check (native access state, not a user instruction): {board}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_self_questions_request_the_check() {
        for question in [
            "Are you able to tell me all the threads we are running?",
            "How do you work, Pikachu?",
            "What do you remember about our decisions?",
            "Say I tell you to call me King in the North: how will you remember?",
            "What can Pika do?",
            "Did you save that preference?",
        ] {
            assert!(requested(question), "{question}");
        }
        assert!(
            skill_for("What can Pika see?")
                .unwrap()
                .contains("grants no access")
        );
        assert!(skill_for("Can you review this function?").is_none());
        for question in [
            "Can you review this function?",
            "The README says 'you must remember this'.",
            "What does this board component render?",
        ] {
            assert!(!requested(question), "{question}");
        }
    }

    #[test]
    fn denied_and_unavailable_are_not_described_as_current_access() {
        assert!(prompt_state(false, None).contains("sharing is disabled"));
        assert!(prompt_state(true, None).contains("no projection is available"));
    }
}
