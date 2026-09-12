use regex::Regex;
use std::collections::BTreeSet;
use std::sync::OnceLock;

/// The one normalized query representation shared by local ranking and the
/// persisted fleet index. Keeping tokenization here prevents a remote search
/// projection from admitting or scoring a different set than `rank_experts`.
#[derive(Clone, Debug)]
pub(crate) struct ExpertQuery {
    pub raw_nonempty: bool,
    pub tokens: Vec<String>,
    pub terms: Vec<String>,
}

impl ExpertQuery {
    pub fn parse(value: &str) -> Self {
        let raw = value
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();
        let tokens = tokens(&raw)
            .into_iter()
            .filter(|token| !ignored_terms().contains(token.as_str()))
            .collect::<Vec<_>>();
        let terms = tokens
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        Self {
            raw_nonempty: !raw.is_empty(),
            tokens,
            terms,
        }
    }

    pub fn phrase(&self) -> String {
        padded(&self.tokens)
    }
}

pub(crate) fn token_sequence(value: &str) -> String {
    padded(&tokens(value))
}

pub(crate) fn field_contains_tokens(field: &str, needle: &[String]) -> bool {
    let haystack = tokens(field);
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

pub(crate) fn field_terms(field: &str) -> BTreeSet<String> {
    tokens(field).into_iter().collect()
}

fn tokens(value: &str) -> Vec<String> {
    static TOKEN: OnceLock<Regex> = OnceLock::new();
    TOKEN
        .get_or_init(|| Regex::new(r"[^\W_]+").expect("static token regex"))
        .find_iter(value)
        .map(|item| item.as_str().to_lowercase())
        .collect()
}

fn ignored_terms() -> &'static BTreeSet<&'static str> {
    static TERMS: OnceLock<BTreeSet<&'static str>> = OnceLock::new();
    TERMS.get_or_init(|| {
        [
            "a", "an", "and", "for", "in", "of", "on", "or", "the", "to", "with",
        ]
        .into_iter()
        .collect()
    })
}

fn padded(tokens: &[String]) -> String {
    if tokens.is_empty() {
        " ".to_owned()
    } else {
        format!(" {} ", tokens.join(" "))
    }
}
