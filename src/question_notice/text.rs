//! Bounded question fingerprints. Raw question text never leaves hook parsing.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

const MAX_QUESTIONS: usize = 8;
const MAX_OPTIONS: usize = 16;
const MAX_TEXT_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuestionFingerprint {
    pub title: String,
    pub options: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "questions", rename_all = "snake_case")]
pub enum QuestionEvidence {
    #[default]
    Unavailable,
    Fingerprints(Vec<QuestionFingerprint>),
}

impl QuestionEvidence {
    pub fn from_tool_input(input: Option<&Value>) -> Self {
        let Some(questions) = input
            .and_then(|input| input.get("questions"))
            .and_then(Value::as_array)
        else {
            return Self::Unavailable;
        };
        if questions.is_empty() || questions.len() > MAX_QUESTIONS {
            return Self::Unavailable;
        }
        let mut bytes = 0;
        let mut result = Vec::new();
        for question in questions {
            let Some(title) = question.get("title").and_then(Value::as_str) else {
                return Self::Unavailable;
            };
            let mut strings = vec![title];
            if let Some(options) = question.get("options").filter(|value| !value.is_null()) {
                let Some(options) = options
                    .as_array()
                    .filter(|options| !options.is_empty() && options.len() <= MAX_OPTIONS)
                else {
                    return Self::Unavailable;
                };
                for option in options {
                    let Some(option) = option.as_str() else {
                        return Self::Unavailable;
                    };
                    strings.push(option);
                }
            }
            for text in &strings {
                bytes += text.len();
                if bytes > MAX_TEXT_BYTES
                    || normalize(text).is_empty()
                    || text
                        .chars()
                        .any(|ch| ch.is_control() && !ch.is_whitespace())
                {
                    return Self::Unavailable;
                }
            }
            result.push(QuestionFingerprint {
                title: fingerprint(title),
                options: strings[1..].iter().map(|text| fingerprint(text)).collect(),
            });
        }
        Self::Fingerprints(result)
    }

    pub fn valid(&self) -> bool {
        match self {
            Self::Unavailable => true,
            Self::Fingerprints(questions) => {
                !questions.is_empty()
                    && questions.len() <= MAX_QUESTIONS
                    && questions.iter().all(|question| {
                        question.options.len() <= MAX_OPTIONS
                            && std::iter::once(&question.title)
                                .chain(&question.options)
                                .all(|digest| {
                                    digest.len() == 64
                                        && digest.bytes().all(|byte| {
                                            byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
                                        })
                                })
                    })
            }
        }
    }

    pub fn matches(&self, title: &str, options: &[String]) -> bool {
        let Self::Fingerprints(questions) = self else {
            return false;
        };
        let title = fingerprint(title);
        let options: Vec<_> = options.iter().map(|option| fingerprint(option)).collect();
        questions
            .iter()
            .any(|question| question.title == title && question.options == options)
    }
}

// Whitespace differences are allowed only for a retaining veto, never a clear
// decision. This includes Japanese soft wrapping; punctuation and case remain.
fn normalize(text: &str) -> String {
    text.chars().filter(|ch| !ch.is_whitespace()).collect()
}

fn fingerprint(text: &str) -> String {
    format!("{:x}", Sha256::digest(normalize(text).as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprints_allow_wrapping_but_require_full_ordered_choices() {
        let evidence = QuestionEvidence::from_tool_input(Some(&serde_json::json!({
            "questions": [{"title":"作業を続けますか？", "options":["Continue now", "Later"]}]
        })));
        assert!(evidence.valid());
        assert!(evidence.matches(
            "作業を\n続けますか？",
            &["Continue\n now".into(), "Later".into()]
        ));
        for (title, options) in [
            (
                "作業を続けますか",
                vec!["Continue now".into(), "Later".into()],
            ),
            ("作業を続けますか？", vec!["Continue now".into()]),
            (
                "作業を続けますか？",
                vec!["Later".into(), "Continue now".into()],
            ),
            (
                "作業を続けますか？",
                vec!["continue now".into(), "Later".into()],
            ),
        ] {
            assert!(!evidence.matches(title, &options));
        }
        let serialized = serde_json::to_string(&evidence).unwrap();
        assert!(!serialized.contains("作業"));
        assert!(!serialized.contains("Continue"));
    }

    #[test]
    fn incomplete_or_unbounded_inputs_have_no_clearance_evidence() {
        assert_eq!(
            QuestionEvidence::from_tool_input(None),
            QuestionEvidence::Unavailable
        );
        for input in [
            serde_json::json!({"questions":[]}),
            serde_json::json!({"questions":[{"title":" "}]}),
            serde_json::json!({"questions":[{"title":"question", "options":[]}]}),
            serde_json::json!({"questions":[{"title":"question", "options":[42]}]}),
            serde_json::json!({"questions":[{"title":"bad\u{0}text"}]}),
            serde_json::json!({"questions":[{"title":"x".repeat(MAX_TEXT_BYTES + 1)}]}),
            serde_json::json!({"questions":vec![serde_json::json!({"title":"q"}); MAX_QUESTIONS+1]}),
        ] {
            assert_eq!(
                QuestionEvidence::from_tool_input(Some(&input)),
                QuestionEvidence::Unavailable
            );
        }
        for question in [
            serde_json::json!({"title":"free text"}),
            serde_json::json!({"title":"free text", "options":null}),
        ] {
            let evidence = QuestionEvidence::from_tool_input(Some(
                &serde_json::json!({"questions":[question]}),
            ));
            assert!(evidence.matches("free text", &[]));
        }
        assert!(
            !QuestionEvidence::Fingerprints(vec![QuestionFingerprint {
                title: "bad".into(),
                options: vec![]
            }])
            .valid()
        );
    }
}
