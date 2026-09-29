//! Read-only Markdown projections of the existing assistant profile. No files
//! are materialized and none of these views becomes an authority for memory.
use crate::{
    assistant_context::CONTINUITY_RULE,
    assistant_guidance,
    assistant_memory::{Scope, Store},
};
use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum View {
    Index,
    Identity,
    Soul,
    Memory,
}

pub(crate) fn render(memory: &Store, scope: &Scope, view: View) -> Result<String> {
    let text = match view {
        View::Index => format!(
            "# Pika profile\n\nRead-only views of the current Pika profile. These are generated on request, not editable files.\n\n- `/profile identity` · IDENTITY.md\n- `/profile soul` · SOUL.md (built-in orientation and active learned guidance)\n- `/profile memory` · MEMORY.md (selected active records)\n\nScope: {}. Nothing here opens a project thread, acknowledges work, or calls a model.",
            label(scope.project.as_deref().unwrap_or("personal"), 256)
        ),
        View::Identity => format!(
            "{}\n\n- **Name:** Pika\n- **Profile ID:** {}\n- **Selected scope:** {}\n\nThe profile ID identifies this durable local assistant. A provider session or open view is not a new profile.",
            header("IDENTITY.md", memory, scope),
            label(memory.profile_id(), 128),
            label(scope.project.as_deref().unwrap_or("personal"), 256),
        ),
        View::Soul => {
            let guidance = assistant_guidance::applicable_guidance(memory, scope, 16)?;
            let mut text = format!(
                "{}\n\n## Built-in orientation\n\n{CONTINUITY_RULE}\n\n## Active learned guidance\n",
                header("SOUL.md", memory, scope)
            );
            if guidance.is_empty() {
                text.push_str("\nNo applicable learned guidance in this bounded view.\n");
            } else {
                for record in guidance {
                    text.push_str(&format!(
                        "\n- {} · {}",
                        label(&record.id, 64),
                        label(&record.body, 240)
                    ));
                }
                text.push('\n');
            }
            text.push_str("\nAt most 16 applicable learned entries are shown. Explicit user instructions and their sources are inspected through `/memory`; this view is not a complete policy inventory. Use `/guidance-off ID` to disable learned guidance.");
            text
        }
        View::Memory => {
            let records = memory.working_set(scope, 16)?;
            let mut text = format!(
                "{}\n\n## Selected active records\n",
                header("MEMORY.md", memory, scope)
            );
            if records.is_empty() {
                text.push_str("\nNo records in this selected working set.\n");
            } else {
                for record in records {
                    text.push_str(&format!(
                        "\n- {} · {:?}, {:?} · {}",
                        label(&record.id, 64),
                        record.kind,
                        record.origin,
                        label(&record.body, 240)
                    ));
                }
                text.push('\n');
            }
            text.push_str("\nThis is a bounded working selection, not all memory. `/memory` opens the scoped archive in pages; `/memory-record ID` shows a full record. Viewing this does not acknowledge source activity.");
            text
        }
    };
    Ok(text)
}

fn header(filename: &str, memory: &Store, scope: &Scope) -> String {
    format!(
        "# {filename}\n\n_Read-only view · profile {} · scope {}_",
        label(memory.profile_id(), 128),
        label(scope.project.as_deref().unwrap_or("personal"), 256)
    )
}

fn label(value: &str, max_chars: usize) -> String {
    crate::fleet::sanitize_terminal_text(value)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max_chars)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        assistant_context::SourceVersion,
        assistant_guidance::{Adaptation, Applicability, GuidanceSpec},
        assistant_memory::{NewRecord, Origin, RecordKind},
    };

    fn selected(name: &str) -> Scope {
        Scope {
            project: Some(name.into()),
            ..Scope::default()
        }
    }

    fn instruction(scope: Scope, body: &str) -> NewRecord {
        NewRecord {
            kind: RecordKind::UserInstruction,
            origin: Origin::Human,
            scope,
            body: body.into(),
            provenance: "submitted_user_message".into(),
            timestamp: 1,
            supersedes: None,
            dependencies: vec![],
            decision_state: None,
            protected_policy: false,
        }
    }

    #[test]
    fn identity_and_memory_are_scoped_current_projections() {
        let temp = tempfile::tempdir().unwrap();
        let mut memory = Store::open(temp.path().join("private/memory.sqlite")).unwrap();
        let own = memory
            .append(instruction(selected("own"), "Keep the planning rationale"))
            .unwrap();
        memory
            .append(instruction(
                selected("other"),
                "Other project's private rule",
            ))
            .unwrap();
        let identity = render(&memory, &selected("own"), View::Identity).unwrap();
        assert!(identity.contains(memory.profile_id()));
        assert!(identity.contains("scope own"));
        let current = render(&memory, &selected("own"), View::Memory).unwrap();
        assert!(current.contains(&own.id));
        assert!(current.contains("Keep the planning rationale"));
        assert!(!current.contains("Other project's private rule"));
        memory.forget(&own.id).unwrap();
        let after = render(&memory, &selected("own"), View::Memory).unwrap();
        assert!(!after.contains(&own.id));
        assert!(after.contains("not all memory"));
        assert!(!temp.path().join("private/provider-home").exists());
    }

    #[test]
    fn soul_uses_only_current_applicable_guidance() {
        let temp = tempfile::tempdir().unwrap();
        let mut memory = Store::open(temp.path().join("private/memory.sqlite")).unwrap();
        let human = memory
            .append(instruction(
                selected("own"),
                "From now on challenge my premise",
            ))
            .unwrap();
        let spec = GuidanceSpec {
            adaptation: Adaptation::Guidance {
                topic: "premise-check".into(),
                instruction: "Challenge my premise when reviewing designs".into(),
                lasting: true,
                when: None,
            },
            sources: vec![SourceVersion {
                id: human.id.clone(),
                revision: memory.source_version(&human.id).unwrap().unwrap(),
            }],
            applicability: Applicability::ScopeWide,
            reason: "Explicit lasting request".into(),
        };
        let learned = memory
            .append(
                assistant_guidance::validate_guidance(&memory, &selected("own"), &spec, 2).unwrap(),
            )
            .unwrap();
        let active = render(&memory, &selected("own"), View::Soul).unwrap();
        assert!(active.contains("Challenge my premise when reviewing designs"));
        assistant_guidance::set_enabled(&mut memory, &learned.id, false, 3).unwrap();
        let disabled = render(&memory, &selected("own"), View::Soul).unwrap();
        assert!(!disabled.contains("Challenge my premise when reviewing designs"));
        assistant_guidance::set_enabled(&mut memory, &learned.id, true, 4).unwrap();
        memory.forget(&human.id).unwrap();
        let forgotten = render(&memory, &selected("own"), View::Soul).unwrap();
        assert!(!forgotten.contains("Challenge my premise when reviewing designs"));
    }
}
