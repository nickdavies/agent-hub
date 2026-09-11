use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, de};
use uuid::Uuid;

use crate::sessions::SessionId;

pub const QUESTION_TEXT_MAX_CHARS: usize = 4096;
pub const QUESTION_HEADER_MAX_CHARS: usize = 30;
pub const QUESTION_MAX_COUNT: usize = 32;
pub const QUESTION_OPTIONS_MAX_COUNT: usize = 64;
pub const QUESTION_OPTION_LABEL_MAX_CHARS: usize = 128;
pub const QUESTION_OPTION_DESCRIPTION_MAX_CHARS: usize = 1024;
pub const QUESTION_PROXY_ID_MAX_CHARS: usize = 128;
pub const QUESTION_SESSION_ID_MAX_CHARS: usize = 256;
pub const QUESTION_SESSION_DISPLAY_NAME_MAX_CHARS: usize = 256;
pub const QUESTION_CWD_MAX_CHARS: usize = 4096;
pub const QUESTION_REQUEST_ID_MAX_CHARS: usize = 256;
pub const QUESTION_PROVIDER_MAX_CHARS: usize = 64;
pub const QUESTION_CUSTOM_ANSWER_MAX_CHARS: usize = 4096;
pub const QUESTION_REASON_MAX_CHARS: usize = 4096;

// ===========================================================================
// Question content types (mirrors opencode's QuestionInfo / QuestionOption)
// ===========================================================================

/// A single selectable option within a question.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QuestionOption {
    #[schemars(length(min = 1, max = 128))]
    pub label: String,
    #[schemars(length(min = 1, max = 1024))]
    pub description: String,
}

impl QuestionOption {
    pub fn new(label: impl Into<String>, description: impl Into<String>) -> Result<Self, String> {
        let option = Self {
            label: label.into(),
            description: description.into(),
        };
        option.validate()?;
        Ok(option)
    }

    pub fn validate(&self) -> Result<(), String> {
        validate_bounded_text(
            "question option label",
            &self.label,
            QUESTION_OPTION_LABEL_MAX_CHARS,
        )?;
        validate_bounded_text(
            "question option description",
            &self.description,
            QUESTION_OPTION_DESCRIPTION_MAX_CHARS,
        )
    }
}

/// One question with its choices.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QuestionInfo {
    #[schemars(length(min = 1, max = 4096))]
    pub question: String,
    /// Short label (≤30 chars) shown as the tab/step heading.
    #[schemars(length(min = 1, max = 30))]
    pub header: String,
    #[schemars(length(max = 64))]
    pub options: Vec<QuestionOption>,
    /// If true, multiple options may be selected simultaneously.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multiple: Option<bool>,
    /// If true (default), a free-text "Type your own answer" input is shown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom: Option<bool>,
}

impl QuestionInfo {
    pub fn new(
        question: impl Into<String>,
        header: impl Into<String>,
        options: Vec<QuestionOption>,
        multiple: Option<bool>,
        custom: Option<bool>,
    ) -> Result<Self, String> {
        let question = Self {
            question: question.into(),
            header: header.into(),
            options,
            multiple,
            custom,
        };
        question.validate()?;
        Ok(question)
    }

    pub fn validate(&self) -> Result<(), String> {
        validate_bounded_text("question text", &self.question, QUESTION_TEXT_MAX_CHARS)?;
        validate_bounded_text("question header", &self.header, QUESTION_HEADER_MAX_CHARS)?;
        if self.options.len() > QUESTION_OPTIONS_MAX_COUNT {
            return Err(format!(
                "question options must contain at most {QUESTION_OPTIONS_MAX_COUNT} items"
            ));
        }
        for option in &self.options {
            option.validate()?;
        }
        Ok(())
    }
}

fn validate_bounded_text(name: &str, value: &str, max_chars: usize) -> Result<(), String> {
    let chars = value.chars().count();
    if chars == 0 || chars > max_chars {
        return Err(format!("{name} must contain 1 to {max_chars} characters"));
    }
    Ok(())
}

// ===========================================================================
// Status enum
// ===========================================================================

/// The lifecycle state of a proxied question.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum QuestionStatus {
    /// Waiting for the operator to answer.
    Pending,
    /// Operator submitted answers.
    Answered {
        /// One `Answer` (array of selected labels) per question, in order.
        #[schemars(schema_with = "question_answers_schema")]
        answers: Vec<Vec<String>>,
    },
    /// Operator dismissed the question.
    Rejected {
        #[schemars(length(max = 4096))]
        reason: Option<String>,
    },
    /// Gateway timed out or was killed without resolving.
    Cancelled,
}

impl QuestionStatus {
    pub fn is_resolved(&self) -> bool {
        !matches!(self, QuestionStatus::Pending)
    }
}

// ===========================================================================
// Gateway → Server: POST /api/v1/hooks/question
// ===========================================================================

/// Request body sent by the gateway when a question needs proxying.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QuestionProxyRequest {
    /// Gateway-generated UUID used as an idempotency key.
    #[schemars(length(min = 1, max = 128))]
    pub id: String,
    #[schemars(with = "String", length(min = 1, max = 256))]
    pub session_id: SessionId,
    #[schemars(length(min = 1, max = 256))]
    pub session_display_name: String,
    #[schemars(length(min = 1, max = 4096))]
    pub cwd: String,
    /// The opencode-native question ID (e.g. "que_01j...").
    #[schemars(length(min = 1, max = 256))]
    pub question_request_id: String,
    #[schemars(length(min = 1, max = 32))]
    pub questions: Vec<QuestionInfo>,
    /// Provider that originated the question (always "opencode" for now).
    #[schemars(length(min = 1, max = 64))]
    pub provider: String,
}

impl QuestionProxyRequest {
    pub fn validate(&self) -> Result<(), String> {
        validate_bounded_text("question proxy id", &self.id, QUESTION_PROXY_ID_MAX_CHARS)?;
        validate_bounded_text(
            "question session id",
            self.session_id.as_str(),
            QUESTION_SESSION_ID_MAX_CHARS,
        )?;
        validate_bounded_text(
            "question session display name",
            &self.session_display_name,
            QUESTION_SESSION_DISPLAY_NAME_MAX_CHARS,
        )?;
        validate_bounded_text("question cwd", &self.cwd, QUESTION_CWD_MAX_CHARS)?;
        validate_bounded_text(
            "question request id",
            &self.question_request_id,
            QUESTION_REQUEST_ID_MAX_CHARS,
        )?;
        if self.questions.is_empty() || self.questions.len() > QUESTION_MAX_COUNT {
            return Err(format!(
                "questions must contain 1 to {QUESTION_MAX_COUNT} items"
            ));
        }
        validate_bounded_text(
            "question provider",
            &self.provider,
            QUESTION_PROVIDER_MAX_CHARS,
        )?;
        for question in &self.questions {
            question.validate()?;
        }
        Ok(())
    }
}

// ===========================================================================
// Server → Gateway: response to POST /api/v1/hooks/question
// ===========================================================================

/// Immediate response returned to the gateway after registering a question.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct QuestionProxyResponse {
    pub id: Uuid,
    #[serde(flatten)]
    pub status: QuestionStatus,
}

impl<'de> Deserialize<'de> for QuestionProxyResponse {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct WireResponse {
            id: Uuid,
            #[serde(flatten)]
            status: QuestionStatus,
        }

        let value = serde_json::Value::deserialize(deserializer)?;
        validate_status_object::<D::Error>(&value, &["id"])?;
        let wire: WireResponse = serde_json::from_value(value).map_err(de::Error::custom)?;
        Ok(Self {
            id: wire.id,
            status: wire.status,
        })
    }
}

// ===========================================================================
// GET /api/v1/questions/{id}/wait — long-poll response
// ===========================================================================

/// Response from the long-poll wait endpoint.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct QuestionWaitResponse {
    #[serde(flatten)]
    pub status: QuestionStatus,
}

impl<'de> Deserialize<'de> for QuestionWaitResponse {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct WireResponse {
            #[serde(flatten)]
            status: QuestionStatus,
        }

        let value = serde_json::Value::deserialize(deserializer)?;
        validate_status_object::<D::Error>(&value, &[])?;
        let wire: WireResponse = serde_json::from_value(value).map_err(de::Error::custom)?;
        Ok(Self {
            status: wire.status,
        })
    }
}

fn validate_status_object<E: de::Error>(
    value: &serde_json::Value,
    additional_fields: &[&str],
) -> Result<(), E> {
    let object = value
        .as_object()
        .ok_or_else(|| E::custom("question status response must be an object"))?;
    let status_type = object
        .get("type")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| E::missing_field("type"))?;
    let status_fields: &[&str] = match status_type {
        "pending" | "cancelled" => &[],
        "answered" => &["answers"],
        "rejected" => &["reason"],
        _ => return Err(E::unknown_variant(status_type, &[])),
    };
    for field in object.keys() {
        if field != "type"
            && !additional_fields.contains(&field.as_str())
            && !status_fields.contains(&field.as_str())
        {
            return Err(E::unknown_field(field, &[]));
        }
    }
    Ok(())
}

// ===========================================================================
// Human → Server: POST /api/v1/questions/{id}/resolve
// ===========================================================================

/// Decision choices when resolving a question.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum QuestionDecision {
    /// Operator submitted answers.
    Answer,
    /// Operator dismissed / rejected the question.
    Reject,
    /// Gateway cancelled (e.g. session ended).
    Cancel,
}

/// Request body for POST /api/v1/questions/{id}/resolve.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QuestionResolveRequest {
    pub decision: QuestionDecision,
    /// Required when `decision == Answer`. One array of selected labels per question.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "optional_question_answers_schema")]
    pub answers: Option<Vec<Vec<String>>>,
    /// Optional reason shown in the dashboard (used with `Reject` or `Cancel`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 4096))]
    pub reason: Option<String>,
}

impl QuestionResolveRequest {
    pub fn validate(&self) -> Result<(), String> {
        if let Some(reason) = &self.reason
            && reason.chars().count() > QUESTION_REASON_MAX_CHARS
        {
            return Err(format!(
                "question resolution reason must contain at most {QUESTION_REASON_MAX_CHARS} characters"
            ));
        }

        match self.decision {
            QuestionDecision::Answer => {
                let answers = self
                    .answers
                    .as_deref()
                    .ok_or_else(|| "answer decision requires answers".to_string())?;
                if self.reason.is_some() {
                    return Err("answer decision does not accept a reason".into());
                }
                validate_answer_shape(answers)
            }
            QuestionDecision::Reject | QuestionDecision::Cancel => {
                if self.answers.is_some() {
                    return Err("reject and cancel decisions do not accept answers".into());
                }
                Ok(())
            }
        }
    }
}

fn validate_answer_shape(answers: &[Vec<String>]) -> Result<(), String> {
    if answers.is_empty() || answers.len() > QUESTION_MAX_COUNT {
        return Err(format!(
            "answers must contain 1 to {QUESTION_MAX_COUNT} items"
        ));
    }
    for (index, selected) in answers.iter().enumerate() {
        if selected.is_empty() || selected.len() > QUESTION_OPTIONS_MAX_COUNT {
            return Err(format!(
                "question {index} must contain 1 to {QUESTION_OPTIONS_MAX_COUNT} selections"
            ));
        }
        let mut unique = std::collections::HashSet::new();
        for answer in selected {
            let chars = answer.chars().count();
            if chars == 0 || chars > QUESTION_CUSTOM_ANSWER_MAX_CHARS {
                return Err(format!(
                    "question {index} selections must contain 1 to {QUESTION_CUSTOM_ANSWER_MAX_CHARS} characters"
                ));
            }
            if !unique.insert(answer) {
                return Err(format!("question {index} contains a duplicate selection"));
            }
        }
    }
    Ok(())
}

fn question_answers_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "array",
        "minItems": 1,
        "maxItems": QUESTION_MAX_COUNT,
        "items": {
            "type": "array",
            "minItems": 1,
            "maxItems": QUESTION_OPTIONS_MAX_COUNT,
            "uniqueItems": true,
            "items": {
                "type": "string",
                "minLength": 1,
                "maxLength": QUESTION_CUSTOM_ANSWER_MAX_CHARS
            }
        }
    })
}

fn optional_question_answers_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    let answers = question_answers_schema(generator);
    schemars::json_schema!({ "anyOf": [answers, { "type": "null" }] })
}

// ===========================================================================
// Gateway stdout: exit-0 output of the `question` subcommand
// ===========================================================================

/// JSON written to stdout by `agent-hub-gateway question` on exit 0.
///
/// Exit codes:
///   0 = answered   — stdout contains this struct
///   1 = explicitly rejected
///   2 = fail-closed malformed input or local failure
///   3 = timed out because the operator was away
///   4 = approvals-pipeline failure
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QuestionGatewayOutput {
    /// One array of selected labels per question, in input order.
    pub answers: Vec<Vec<String>>,
}

/// A pending (or resolved) question as stored by the server.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PendingQuestion {
    /// Server-assigned UUID.
    pub id: Uuid,
    /// Gateway-generated idempotency key (matches `QuestionProxyRequest::id`).
    pub request_id: String,
    pub session_id: SessionId,
    pub session_display_name: String,
    pub project: String,
    /// The opencode-native question ID (forwarded to the plugin for reply/reject calls).
    pub question_request_id: String,
    pub questions: Vec<QuestionInfo>,
    pub provider: String,
    pub created_at: DateTime<Utc>,
    pub status: QuestionStatus,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_option() -> QuestionOption {
        QuestionOption {
            label: "Yes".into(),
            description: "Continue".into(),
        }
    }

    fn valid_question() -> QuestionInfo {
        QuestionInfo {
            question: "Continue?".into(),
            header: "Decision".into(),
            options: vec![valid_option()],
            multiple: None,
            custom: None,
        }
    }

    fn valid_request() -> QuestionProxyRequest {
        QuestionProxyRequest {
            id: "request-1".into(),
            session_id: SessionId::new("session-1"),
            session_display_name: "Session".into(),
            cwd: "/workspace".into(),
            question_request_id: "question-1".into(),
            questions: vec![valid_question()],
            provider: "opencode".into(),
        }
    }

    #[test]
    fn question_contract_accepts_boundaries_and_counts_unicode_characters() {
        let question = QuestionInfo::new(
            "🦀".repeat(QUESTION_TEXT_MAX_CHARS),
            "界".repeat(QUESTION_HEADER_MAX_CHARS),
            vec![
                QuestionOption::new(
                    "é".repeat(QUESTION_OPTION_LABEL_MAX_CHARS),
                    "🙂".repeat(QUESTION_OPTION_DESCRIPTION_MAX_CHARS),
                )
                .unwrap();
                QUESTION_OPTIONS_MAX_COUNT
            ],
            Some(true),
            Some(false),
        )
        .unwrap();

        assert_eq!(question.question.chars().count(), QUESTION_TEXT_MAX_CHARS);
        assert_eq!(question.header.chars().count(), QUESTION_HEADER_MAX_CHARS);
        assert_eq!(question.options.len(), QUESTION_OPTIONS_MAX_COUNT);
        QuestionInfo::new("Custom answer?", "Custom", vec![], None, Some(true)).unwrap();
    }

    #[test]
    fn question_contract_rejects_empty_and_oversize_fields() {
        assert!(QuestionOption::new("", "Description").is_err());
        assert!(QuestionInfo::new("", "Header", vec![], None, None).is_err());

        for (field, value) in [
            ("question", String::new()),
            ("question", "q".repeat(QUESTION_TEXT_MAX_CHARS + 1)),
            ("header", String::new()),
            ("header", "h".repeat(QUESTION_HEADER_MAX_CHARS + 1)),
        ] {
            let mut question = valid_question();
            match field {
                "question" => question.question = value,
                "header" => question.header = value,
                _ => unreachable!(),
            }
            assert!(question.validate().is_err(), "accepted invalid {field}");
        }

        let mut question = valid_question();
        question.options = vec![valid_option(); QUESTION_OPTIONS_MAX_COUNT + 1];
        assert!(question.validate().is_err(), "accepted too many options");

        for (field, value) in [
            ("label", String::new()),
            ("label", "l".repeat(QUESTION_OPTION_LABEL_MAX_CHARS + 1)),
            ("description", String::new()),
            (
                "description",
                "d".repeat(QUESTION_OPTION_DESCRIPTION_MAX_CHARS + 1),
            ),
        ] {
            let mut option = valid_option();
            match field {
                "label" => option.label = value,
                "description" => option.description = value,
                _ => unreachable!(),
            }
            assert!(option.validate().is_err(), "accepted invalid {field}");
        }
    }

    #[test]
    fn question_request_accepts_exact_unicode_boundaries() {
        let request = QuestionProxyRequest {
            id: "🦀".repeat(QUESTION_PROXY_ID_MAX_CHARS),
            session_id: SessionId::new("界".repeat(QUESTION_SESSION_ID_MAX_CHARS)),
            session_display_name: "é".repeat(QUESTION_SESSION_DISPLAY_NAME_MAX_CHARS),
            cwd: "🙂".repeat(QUESTION_CWD_MAX_CHARS),
            question_request_id: "🦀".repeat(QUESTION_REQUEST_ID_MAX_CHARS),
            questions: vec![valid_question(); QUESTION_MAX_COUNT],
            provider: "界".repeat(QUESTION_PROVIDER_MAX_CHARS),
        };

        request.validate().unwrap();
    }

    #[test]
    fn question_request_rejects_empty_and_oversize_top_level_fields() {
        for (field, max_chars) in [
            ("id", QUESTION_PROXY_ID_MAX_CHARS),
            ("session_id", QUESTION_SESSION_ID_MAX_CHARS),
            (
                "session_display_name",
                QUESTION_SESSION_DISPLAY_NAME_MAX_CHARS,
            ),
            ("cwd", QUESTION_CWD_MAX_CHARS),
            ("question_request_id", QUESTION_REQUEST_ID_MAX_CHARS),
            ("provider", QUESTION_PROVIDER_MAX_CHARS),
        ] {
            for value in [String::new(), "界".repeat(max_chars + 1)] {
                let mut request = valid_request();
                match field {
                    "id" => request.id = value,
                    "session_id" => request.session_id = SessionId::new(value),
                    "session_display_name" => request.session_display_name = value,
                    "cwd" => request.cwd = value,
                    "question_request_id" => request.question_request_id = value,
                    "provider" => request.provider = value,
                    _ => unreachable!(),
                }
                assert!(request.validate().is_err(), "accepted invalid {field}");
            }
        }

        for questions in [vec![], vec![valid_question(); QUESTION_MAX_COUNT + 1]] {
            let mut request = valid_request();
            request.questions = questions;
            assert!(request.validate().is_err(), "accepted invalid questions");
        }
    }

    #[test]
    fn question_schema_matches_runtime_bounds() {
        let schema = serde_json::to_value(schemars::schema_for!(QuestionProxyRequest)).unwrap();
        let request = &schema["properties"];
        let question = &schema["$defs"]["QuestionInfo"]["properties"];
        let option = &schema["$defs"]["QuestionOption"]["properties"];

        for (field, max_chars) in [
            ("id", QUESTION_PROXY_ID_MAX_CHARS),
            ("session_id", QUESTION_SESSION_ID_MAX_CHARS),
            (
                "session_display_name",
                QUESTION_SESSION_DISPLAY_NAME_MAX_CHARS,
            ),
            ("cwd", QUESTION_CWD_MAX_CHARS),
            ("question_request_id", QUESTION_REQUEST_ID_MAX_CHARS),
            ("provider", QUESTION_PROVIDER_MAX_CHARS),
        ] {
            assert_eq!(request[field]["minLength"], 1, "{field} minLength");
            assert_eq!(request[field]["maxLength"], max_chars, "{field} maxLength");
        }
        assert_eq!(request["questions"]["minItems"], 1);
        assert_eq!(request["questions"]["maxItems"], QUESTION_MAX_COUNT);
        assert_eq!(question["question"]["minLength"], 1);
        assert_eq!(question["question"]["maxLength"], QUESTION_TEXT_MAX_CHARS);
        assert_eq!(question["header"]["minLength"], 1);
        assert_eq!(question["header"]["maxLength"], QUESTION_HEADER_MAX_CHARS);
        assert_eq!(question["options"]["maxItems"], QUESTION_OPTIONS_MAX_COUNT);
        assert_eq!(option["label"]["minLength"], 1);
        assert_eq!(
            option["label"]["maxLength"],
            QUESTION_OPTION_LABEL_MAX_CHARS
        );
        assert_eq!(option["description"]["minLength"], 1);
        assert_eq!(
            option["description"]["maxLength"],
            QUESTION_OPTION_DESCRIPTION_MAX_CHARS
        );
    }

    #[test]
    fn question_requests_reject_unknown_fields_at_every_level() {
        let request = serde_json::json!({
            "id": "request-1",
            "session_id": "session-1",
            "session_display_name": "Session",
            "cwd": "/workspace",
            "question_request_id": "question-1",
            "questions": [{
                "question": "Continue?",
                "header": "Decision",
                "options": [{
                    "label": "Yes",
                    "description": "Continue"
                }]
            }],
            "provider": "opencode"
        });
        for path in ["request", "question", "option"] {
            let mut invalid = request.clone();
            match path {
                "request" => invalid["unexpected"] = true.into(),
                "question" => invalid["questions"][0]["unexpected"] = true.into(),
                "option" => invalid["questions"][0]["options"][0]["unexpected"] = true.into(),
                _ => unreachable!(),
            }
            assert!(
                serde_json::from_value::<QuestionProxyRequest>(invalid).is_err(),
                "accepted unknown {path} field"
            );
        }

        let resolution = serde_json::json!({
            "decision": "answer",
            "answers": [["Yes"]],
            "unexpected": true
        });
        assert!(serde_json::from_value::<QuestionResolveRequest>(resolution).is_err());
    }

    #[test]
    fn question_status_rejects_unknown_variant_fields() {
        assert!(
            serde_json::from_value::<QuestionStatus>(serde_json::json!({
                "type": "answered",
                "answers": [["Yes"]],
                "unexpected": true
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<QuestionProxyResponse>(serde_json::json!({
                "id": Uuid::nil(),
                "type": "pending",
                "unexpected": true
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<QuestionWaitResponse>(serde_json::json!({
                "type": "answered",
                "answers": [["Yes"]],
                "unexpected": true
            }))
            .is_err()
        );
    }

    #[test]
    fn question_resolution_bounds_unicode_and_selection_shape() {
        let valid = QuestionResolveRequest {
            decision: QuestionDecision::Answer,
            answers: Some(vec![vec!["界".repeat(QUESTION_CUSTOM_ANSWER_MAX_CHARS)]]),
            reason: None,
        };
        valid.validate().unwrap();

        let duplicate = QuestionResolveRequest {
            decision: QuestionDecision::Answer,
            answers: Some(vec![vec!["same".into(), "same".into()]]),
            reason: None,
        };
        assert!(duplicate.validate().is_err());

        let too_many = QuestionResolveRequest {
            decision: QuestionDecision::Answer,
            answers: Some(vec![vec!["answer".into(); QUESTION_OPTIONS_MAX_COUNT + 1]]),
            reason: None,
        };
        assert!(too_many.validate().is_err());

        let oversized_answer = QuestionResolveRequest {
            decision: QuestionDecision::Answer,
            answers: Some(vec![vec![
                "界".repeat(QUESTION_CUSTOM_ANSWER_MAX_CHARS + 1),
            ]]),
            reason: None,
        };
        assert!(oversized_answer.validate().is_err());

        let valid_reason = QuestionResolveRequest {
            decision: QuestionDecision::Reject,
            answers: None,
            reason: Some("🙂".repeat(QUESTION_REASON_MAX_CHARS)),
        };
        valid_reason.validate().unwrap();
        let oversized_reason = QuestionResolveRequest {
            reason: Some("🙂".repeat(QUESTION_REASON_MAX_CHARS + 1)),
            ..valid_reason
        };
        assert!(oversized_reason.validate().is_err());
    }

    #[test]
    fn question_resolution_schema_matches_runtime_bounds() {
        let schema = serde_json::to_value(schemars::schema_for!(QuestionResolveRequest)).unwrap();
        let properties = &schema["properties"];
        let answers = &properties["answers"]["anyOf"][0];
        assert_eq!(answers["minItems"], 1);
        assert_eq!(answers["maxItems"], QUESTION_MAX_COUNT);
        assert_eq!(answers["items"]["minItems"], 1);
        assert_eq!(answers["items"]["maxItems"], QUESTION_OPTIONS_MAX_COUNT);
        assert_eq!(answers["items"]["uniqueItems"], true);
        assert_eq!(answers["items"]["items"]["minLength"], 1);
        assert_eq!(
            answers["items"]["items"]["maxLength"],
            QUESTION_CUSTOM_ANSWER_MAX_CHARS
        );
        assert_eq!(properties["reason"]["maxLength"], QUESTION_REASON_MAX_CHARS);
    }
}
