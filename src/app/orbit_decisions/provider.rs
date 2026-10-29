//! Decision requests, validated answers, and the providers that produce them.
//! Shapes follow TypeSafe's System One API: `state` plus named `questions`, each
//! `noul` (a probability that the statement holds), `choice` (probabilities over
//! named options), or `score` (probabilities over ordered levels from 0).
use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Rounded probabilities from the API may sum slightly off 1.
const SUM_TOLERANCE: f64 = 0.05;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub(crate) enum Question {
    Noul {
        instructions: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        criteria: Option<BTreeMap<String, String>>,
    },
    Choice {
        instructions: String,
        criteria: BTreeMap<String, String>,
    },
    Score {
        instructions: String,
        /// Level descriptions, lowest first; levels are numbered from 0.
        criteria: Vec<String>,
    },
}

impl Question {
    pub(crate) fn noul(instructions: &str) -> Self {
        Self::Noul {
            instructions: instructions.into(),
            criteria: None,
        }
    }

    pub(crate) fn choice(instructions: &str, options: &[(&str, &str)]) -> Self {
        Self::Choice {
            instructions: instructions.into(),
            criteria: options
                .iter()
                .map(|(name, text)| ((*name).to_owned(), (*text).to_owned()))
                .collect(),
        }
    }

    pub(crate) fn score(instructions: &str, levels: &[&str]) -> Self {
        Self::Score {
            instructions: instructions.into(),
            criteria: levels.iter().map(|level| (*level).to_owned()).collect(),
        }
    }
}

/// What to decide: the state the questions are about and the questions by name.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct DecisionRequest {
    pub(crate) state: Value,
    pub(crate) questions: BTreeMap<String, Question>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub(crate) enum Answer {
    Noul {
        noul: f64,
    },
    Choice {
        choice: String,
        #[serde(default)]
        confidence: f64,
        probabilities: BTreeMap<String, f64>,
    },
    Score {
        score: f64,
        #[serde(default)]
        confidence: f64,
        #[serde(default)]
        legend: BTreeMap<String, String>,
        probabilities: BTreeMap<String, f64>,
    },
}

/// Validated answers, by question name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct DecisionAnswers(pub(crate) BTreeMap<String, Answer>);

impl DecisionAnswers {
    pub(crate) fn noul(&self, name: &str) -> Option<f64> {
        match self.0.get(name)? {
            Answer::Noul { noul } => Some(*noul),
            _ => None,
        }
    }

    /// Probability of `option` in choice `name`.
    pub(crate) fn choice_p(&self, name: &str, option: &str) -> Option<f64> {
        match self.0.get(name)? {
            Answer::Choice { probabilities, .. } => Some(*probabilities.get(option)?),
            _ => None,
        }
    }

    /// The most probable option of choice `name` and its probability.
    pub(crate) fn top_choice(&self, name: &str) -> Option<(String, f64)> {
        match self.0.get(name)? {
            Answer::Choice { probabilities, .. } => probabilities
                .iter()
                .max_by(|a, b| a.1.total_cmp(b.1))
                .map(|(option, p)| (option.clone(), *p)),
            _ => None,
        }
    }

    /// Mean level of score `name`.
    pub(crate) fn score_mean(&self, name: &str) -> Option<f64> {
        match self.0.get(name)? {
            Answer::Score { score, .. } => Some(*score),
            _ => None,
        }
    }

    /// Probability mass of score `name` in `levels`.
    pub(crate) fn score_mass(&self, name: &str, levels: std::ops::RangeInclusive<usize>) -> f64 {
        match self.0.get(name) {
            Some(Answer::Score { probabilities, .. }) => levels
                .filter_map(|level| probabilities.get(&level.to_string()))
                .sum(),
            _ => 0.0,
        }
    }
}

/// Why no answer is available. Messages never carry key material.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum DecisionError {
    /// The rules-only provider never calls out.
    Disabled,
    /// No usable API key (unset variable, unreadable or unsafe key file).
    Key(String),
    Timeout,
    /// curl could not run or the request did not complete.
    Transport(String),
    /// The API answered with an error status.
    Http {
        status: u16,
        error_type: Option<String>,
        message: Option<String>,
    },
    /// The response did not answer every question with a valid value.
    Malformed(String),
}

impl fmt::Display for DecisionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disabled => write!(f, "decision provider disabled"),
            Self::Key(message) => write!(f, "API key unavailable: {message}"),
            Self::Timeout => write!(f, "timed out"),
            Self::Transport(message) => write!(f, "request failed: {message}"),
            Self::Http {
                status,
                error_type,
                message,
            } => {
                write!(f, "HTTP {status}")?;
                if let Some(kind) = error_type {
                    write!(f, " {kind}")?;
                }
                if let Some(message) = message {
                    write!(f, ": {message}")?;
                }
                Ok(())
            }
            Self::Malformed(message) => write!(f, "malformed response: {message}"),
        }
    }
}

pub(crate) trait DecisionProvider: Send + Sync {
    /// Short name used in notices and the journal.
    fn name(&self) -> &'static str;

    /// Blocking; called only on a decision thread, never on the event loop.
    fn evaluate(&self, request: DecisionRequest) -> Result<DecisionAnswers, DecisionError>;
}

/// The default: decisions come from the rules alone.
pub(crate) struct RulesOnly;

impl DecisionProvider for RulesOnly {
    fn name(&self) -> &'static str {
        "rules"
    }

    fn evaluate(&self, _request: DecisionRequest) -> Result<DecisionAnswers, DecisionError> {
        Err(DecisionError::Disabled)
    }
}

#[cfg(test)]
type ScriptFn = dyn Fn(&DecisionRequest) -> Result<DecisionAnswers, DecisionError> + Send + Sync;

/// Answers produced by a closure, for tests.
#[cfg(test)]
pub(crate) struct Scripted {
    pub(crate) answer: Box<ScriptFn>,
    pub(crate) calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

#[cfg(test)]
impl Scripted {
    pub(crate) fn new(
        answer: impl Fn(&DecisionRequest) -> Result<DecisionAnswers, DecisionError>
            + Send
            + Sync
            + 'static,
    ) -> Self {
        Self {
            answer: Box::new(answer),
            calls: Default::default(),
        }
    }

    /// Answers every request with the `answers` object of a recorded response.
    pub(crate) fn fixture(response: &str) -> Self {
        let parsed = parse_answers(response.as_bytes(), None).expect("valid fixture");
        Self::new(move |_| Ok(parsed.clone()))
    }
}

#[cfg(test)]
impl DecisionProvider for Scripted {
    fn name(&self) -> &'static str {
        "scripted"
    }

    fn evaluate(&self, request: DecisionRequest) -> Result<DecisionAnswers, DecisionError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let answers = (self.answer)(&request)?;
        validate(&request, &answers)?;
        Ok(answers)
    }
}

#[derive(Deserialize)]
struct Envelope {
    answers: BTreeMap<String, Answer>,
}

/// Parses a success body; with `request`, also checks it answers every question.
pub(crate) fn parse_answers(
    body: &[u8],
    request: Option<&DecisionRequest>,
) -> Result<DecisionAnswers, DecisionError> {
    let envelope: Envelope = serde_json::from_slice(body)
        .map_err(|error| DecisionError::Malformed(format!("invalid JSON: {error}")))?;
    let answers = DecisionAnswers(envelope.answers);
    for (name, answer) in &answers.0 {
        check_values(name, answer)?;
    }
    if let Some(request) = request {
        validate(request, &answers)?;
    }
    Ok(answers)
}

/// The error body of a non-2xx response: `{"detail":{"error_type","message"}}`,
/// or a 422 validation list whose details are dropped.
pub(crate) fn http_error(status: u16, body: &[u8]) -> DecisionError {
    let detail = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| value.get("detail").cloned());
    let field = |key: &str| {
        detail
            .as_ref()
            .and_then(|detail| detail.get(key))
            .and_then(Value::as_str)
            .map(|text| super::journal::bounded(text, 200))
    };
    DecisionError::Http {
        status,
        error_type: field("error_type"),
        message: field("message"),
    }
}

fn probability(name: &str, value: f64) -> Result<(), DecisionError> {
    if value.is_finite() && (0.0..=1.0).contains(&value) {
        Ok(())
    } else {
        Err(DecisionError::Malformed(format!(
            "{name}: probability {value} out of range"
        )))
    }
}

fn distribution(name: &str, probabilities: &BTreeMap<String, f64>) -> Result<(), DecisionError> {
    for value in probabilities.values() {
        probability(name, *value)?;
    }
    let sum: f64 = probabilities.values().sum();
    if (sum - 1.0).abs() > SUM_TOLERANCE {
        return Err(DecisionError::Malformed(format!(
            "{name}: probabilities sum to {sum:.2}"
        )));
    }
    Ok(())
}

fn check_values(name: &str, answer: &Answer) -> Result<(), DecisionError> {
    match answer {
        Answer::Noul { noul } => probability(name, *noul),
        Answer::Choice { probabilities, .. } => distribution(name, probabilities),
        Answer::Score {
            score,
            probabilities,
            ..
        } => {
            if !score.is_finite() || *score < 0.0 {
                return Err(DecisionError::Malformed(format!("{name}: invalid score")));
            }
            distribution(name, probabilities)
        }
    }
}

/// Every question is answered with its own type, choice options, and levels.
fn validate(request: &DecisionRequest, answers: &DecisionAnswers) -> Result<(), DecisionError> {
    for (name, question) in &request.questions {
        let answer = answers
            .0
            .get(name)
            .ok_or_else(|| DecisionError::Malformed(format!("{name}: not answered")))?;
        let matches = match (question, answer) {
            (Question::Noul { .. }, Answer::Noul { .. }) => true,
            (Question::Choice { criteria, .. }, Answer::Choice { probabilities, .. }) => {
                probabilities.keys().eq(criteria.keys())
            }
            (Question::Score { criteria, .. }, Answer::Score { probabilities, .. }) => {
                let mut levels: Vec<usize> = probabilities
                    .keys()
                    .filter_map(|level| level.parse().ok())
                    .collect();
                levels.sort_unstable();
                levels.len() == probabilities.len() && levels.iter().copied().eq(0..criteria.len())
            }
            _ => false,
        };
        if !matches {
            return Err(DecisionError::Malformed(format!(
                "{name}: answer does not match the question"
            )));
        }
    }
    Ok(())
}
