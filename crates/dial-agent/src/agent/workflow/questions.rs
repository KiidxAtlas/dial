//! Human questions are input, never tool authorization. All surfaces share this bounded protocol.
use crate::config::types::Config;
use crate::runtime::tool_output::ToolOutput;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    path::Path,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

pub const MAX_QUESTIONS: usize = 32;
pub const MAX_OPTIONS: usize = 64;
pub const MAX_TEXT: usize = 64 * 1024;
pub const MAX_IMAGES: usize = 16;
const MAX_REQUEST_BYTES: usize = 256 * 1024;
static NEXT: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuestionOption {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Question {
    pub id: String,
    pub question: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    pub options: Vec<QuestionOption>,
    #[serde(default, deserialize_with = "null_as_false")]
    pub multi: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommended: Option<usize>,
}

/// Strict-mode tool calls send optional fields as `null`; treat that as the default `false`.
fn null_as_false<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
    Ok(Option::<bool>::deserialize(deserializer)?.unwrap_or(false))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    questions: Vec<Question>,
}
#[derive(Clone, Debug, Serialize)]
pub struct QuestionRequest {
    pub question_id: String,
    pub generation: u64,
    /// The actual conversation identity; an ephemeral child's identity is never advertised as durable.
    pub session_id: String,
    /// Live owner identity, supplied by the controller bridge, never by model arguments.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner_task_id: Option<String>,
    pub questions: Vec<Question>,
    pub timeout_seconds: u64,
    pub notify: bool,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct QuestionAnswer {
    pub id: String,
    #[serde(default)]
    pub selected_options: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_input: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default)]
    pub timed_out: bool,
    /// Verified human-supplied references, not model-supplied filesystem paths.
    #[serde(skip)]
    pub custom_input_images: Vec<Value>,
    #[serde(skip)]
    pub note_images: Vec<Value>,
}
#[derive(Clone, Debug)]
pub enum QuestionOutcome {
    Answered(Vec<QuestionAnswer>),
    Chat,
    Cancelled,
    Disconnected,
    Unsupported(String),
}

pub fn schema() -> Value {
    json!({"type":"function","function":{"name":"ask","description":"Ask the actual user one or more clarifying questions. Supports single/multiple choices, descriptions, text previews, Other/custom input, notes and Chat about this. Recommendations are badges, not human answers. Asking grants no filesystem or network permission. Runs exclusively; unattended surfaces abort honestly.","parameters":{"type":"object","properties":{"questions":{"type":"array","minItems":1,"maxItems":MAX_QUESTIONS,"items":{"type":"object","properties":{"id":{"type":"string","minLength":1,"maxLength":256},"question":{"type":"string","minLength":1,"maxLength":MAX_TEXT},"header":{"type":"string","maxLength":256},"options":{"type":"array","maxItems":MAX_OPTIONS,"items":{"type":"object","properties":{"label":{"type":"string","minLength":1,"maxLength":1024},"description":{"type":"string","maxLength":MAX_TEXT},"preview":{"type":"string","maxLength":MAX_TEXT}},"required":["label"],"additionalProperties":false}},"multi":{"type":"boolean"},"recommended":{"type":"integer","minimum":0,"maximum":MAX_OPTIONS-1}},"required":["id","question","options"],"additionalProperties":false}}},"required":["questions"],"additionalProperties":false}}})
}

/// Replace CR runs with one space and strip terminal/control injection, retaining ordinary newlines.
pub fn sanitize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut carriage = false;
    for ch in text.chars() {
        if ch == '\r' {
            if !carriage {
                out.push(' ');
            }
            carriage = true;
            continue;
        }
        carriage = false;
        if !ch.is_control() || ch == '\n' || ch == '\t' {
            out.push(ch);
        }
    }
    out
}
fn bounded(text: &mut String, max: usize, required: bool) -> Result<(), String> {
    if text.len() > max {
        return Err(format!("question text exceeds {max} bytes"));
    }
    if text
        .chars()
        .any(|ch| ch == '\r' || (ch.is_control() && ch != '\n' && ch != '\t'))
    {
        *text = sanitize(text);
    }
    if required && text.trim().is_empty() {
        return Err("question ids, questions and option labels must be nonempty".into());
    }
    Ok(())
}
pub fn prepare(
    args: &Value,
    config: &Config,
    plan_mode: bool,
    session_id: &str,
) -> Result<QuestionRequest, String> {
    if !config.ask.enabled {
        return Err("ask tool is disabled by ask.enabled".into());
    }
    if args.to_string().len() > MAX_REQUEST_BYTES {
        return Err("ask arguments exceed 256 KiB".into());
    }
    let mut args: Arguments =
        serde_json::from_value(args.clone()).map_err(|e| format!("invalid ask arguments: {e}"))?;
    if args.questions.is_empty() || args.questions.len() > MAX_QUESTIONS {
        return Err("ask needs 1..32 questions".into());
    }
    let mut ids = HashSet::new();
    for question in &mut args.questions {
        bounded(&mut question.id, 256, true)?;
        bounded(&mut question.question, MAX_TEXT, true)?;
        if !ids.insert(question.id.clone()) {
            return Err("question ids must be unique after sanitization".into());
        }
        if let Some(header) = &mut question.header {
            bounded(header, 256, false)?;
        }
        if question.options.len() > MAX_OPTIONS {
            return Err("a question supports at most 64 options".into());
        }
        if question
            .recommended
            .is_some_and(|index| index >= question.options.len())
        {
            return Err("recommended must be an offered option's 0-based index".into());
        }
        let mut labels = HashSet::new();
        for option in &mut question.options {
            bounded(&mut option.label, 1024, true)?;
            if matches!(
                option.label.as_str(),
                "Other (type your own)"
                    | "Chat about this"
                    | "Next →"
                    | "Done"
                    | "Done selecting"
                    | "✓ Done selecting"
            ) {
                return Err(format!(
                    "option label {:?} is reserved for the runtime",
                    option.label
                ));
            }
            if !labels.insert(option.label.clone()) {
                return Err(
                    "option labels must be unique within a question after sanitization".into(),
                );
            }
            if let Some(text) = &mut option.description {
                bounded(text, MAX_TEXT, false)?;
            }
            if let Some(text) = &mut option.preview {
                bounded(text, MAX_TEXT, false)?;
            }
        }
    }
    let generation = NEXT
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
        .map_err(|_| "question identity counter exhausted")?;
    Ok(QuestionRequest {
        question_id: format!("question-{:032x}-{generation}", rand::random::<u128>()),
        generation,
        session_id: session_id.to_owned(),
        owner_task_id: None,
        questions: args.questions,
        timeout_seconds: if plan_mode {
            0
        } else {
            config.ask.timeout_seconds
        },
        notify: config.ask.notify,
    })
}

pub fn empty_answers(request: &QuestionRequest) -> Vec<QuestionAnswer> {
    request
        .questions
        .iter()
        .map(|q| QuestionAnswer {
            id: q.id.clone(),
            ..Default::default()
        })
        .collect()
}
/// Only the input owner calls this after its configured presentation deadline actually expires.
pub fn expire(request: &QuestionRequest, mut answers: Vec<QuestionAnswer>) -> QuestionOutcome {
    for (question, answer) in request.questions.iter().zip(&mut answers) {
        if answer.selected_options.is_empty() && answer.custom_input.is_none() {
            if let Some(option) = question.options.get(question.recommended.unwrap_or(0)) {
                answer.selected_options.push(option.label.clone());
            }
            answer.timed_out = true;
        }
    }
    QuestionOutcome::Answered(answers)
}

pub fn validate_answers(
    request: &QuestionRequest,
    answers: &mut [QuestionAnswer],
) -> Result<(), String> {
    if answers.len() != request.questions.len() {
        return Err("answer count must match the requested questions".into());
    }
    let mut bytes = 0usize;
    let mut images = 0usize;
    for (question, answer) in request.questions.iter().zip(answers) {
        if answer.id != question.id {
            return Err("answers must match question ids in requested order".into());
        }
        if !question.multi && answer.selected_options.len() > 1 {
            return Err("single-choice question accepts at most one option".into());
        }
        let mut selected = HashSet::new();
        for label in &answer.selected_options {
            if !question.options.iter().any(|option| option.label == *label)
                || !selected.insert(label)
            {
                return Err("answer contains an unknown or duplicate option".into());
            }
            bytes += label.len();
        }
        for text in [&mut answer.custom_input, &mut answer.note]
            .into_iter()
            .flatten()
        {
            bounded(text, MAX_TEXT, false)?;
            bytes += text.len();
        }
        if !question.multi
            && answer.selected_options.is_empty()
            && answer.custom_input.is_none()
            && !answer.timed_out
        {
            return Err("single-choice answer needs an option or custom input".into());
        }
        images += answer.custom_input_images.len() + answer.note_images.len();
    }
    if bytes > MAX_REQUEST_BYTES || images > MAX_IMAGES {
        return Err("answer exceeds 256 KiB or 16 images".into());
    }
    Ok(())
}
fn shift_markers(text: &mut Option<String>, offset: usize, count: usize) {
    let Some(text) = text else {
        return;
    };
    // Descending replacement avoids recursively renumbering an earlier replacement.
    for index in (1..=count).rev() {
        *text = text.replace(
            &format!("[Image #{index}]"),
            &format!("[Image #{}]", offset + index),
        );
    }
}
pub fn output(
    request: &QuestionRequest,
    outcome: QuestionOutcome,
    cancel: &AtomicBool,
) -> ToolOutput {
    let outcome = if cancel.load(Ordering::SeqCst)
        && !matches!(
            &outcome,
            QuestionOutcome::Disconnected | QuestionOutcome::Unsupported(_)
        ) {
        QuestionOutcome::Cancelled
    } else {
        outcome
    };
    match outcome {
        QuestionOutcome::Answered(mut answers) => {
            if let Err(error) = validate_answers(request, &mut answers) {
                return ToolOutput::err(error);
            }
            for image in answers
                .iter()
                .flat_map(|answer| answer.custom_input_images.iter().chain(&answer.note_images))
            {
                if let Err(error) = crate::llm::images::verified_bytes(image) {
                    return ToolOutput::err(format!("answer image unavailable: {error:#}"));
                }
            }
            let mut images = Vec::new();
            for answer in &mut answers {
                shift_markers(
                    &mut answer.custom_input,
                    images.len(),
                    answer.custom_input_images.len(),
                );
                images.append(&mut answer.custom_input_images);
                shift_markers(&mut answer.note, images.len(), answer.note_images.len());
                images.append(&mut answer.note_images);
            }
            let results: Vec<Value> = request
                .questions
                .iter()
                .zip(&answers)
                .map(|(q, a)| {
                    let mut value = json!(a);
                    value["question"] = json!(q.question);
                    value["options"] =
                        json!(q.options.iter().map(|o| &o.label).collect::<Vec<_>>());
                    value["multi"] = json!(q.multi);
                    value
                })
                .collect();
            let image_notices = images.iter().enumerate().map(|(index, image)| {
                let root = image["root"].as_str().unwrap_or("");
                let path = image["path"].as_str().unwrap_or("");
                format!("[Image #{}: explicitly supplied human answer attachment: {}. This reference authorizes only this exact verified image, not arbitrary filesystem access.]", index + 1, sanitize(&Path::new(root).join(path).to_string_lossy()))
            }).collect::<Vec<_>>().join("\n");
            ToolOutput {
                execution: Default::default(),
                text: format!(
                    "User answers:\n{}{}{}",
                    results
                        .iter()
                        .map(Value::to_string)
                        .collect::<Vec<_>>()
                        .join("\n"),
                    if image_notices.is_empty() { "" } else { "\n" },
                    image_notices
                ),
                is_error: false,
                data: Some(
                    json!({"questions":request.questions,"results":results,"dial_images":images}),
                ),
            }
        }
        QuestionOutcome::Chat => ToolOutput {
            execution: Default::default(),
            text: format!(
                "User chose to chat about this instead of answering.\nQuestions asked:\n{}",
                request
                    .questions
                    .iter()
                    .map(|q| q.question.as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
            is_error: false,
            data: Some(json!({"chatRedirect":true,"questions":request.questions})),
        },
        outcome => {
            cancel.store(true, Ordering::SeqCst);
            let (reason, outcome) = match outcome {
                QuestionOutcome::Cancelled => (
                    "Ask input was cancelled; no answer was committed".to_owned(),
                    "cancelled",
                ),
                QuestionOutcome::Disconnected => (
                    "Ask input disconnected before the owner committed an answer".to_owned(),
                    "disconnected",
                ),
                QuestionOutcome::Unsupported(reason) => (reason, "unsupported"),
                _ => unreachable!(),
            };
            ToolOutput {
                execution: Default::default(),
                text: reason.clone(),
                is_error: true,
                data: Some(
                    json!({"questions":request.questions,"cancelled":true,"outcome":outcome,"reason":reason}),
                ),
            }
        }
    }
}

/// Image paths arrive only through actual human input. They do not authorize model path reads.
pub fn attach(
    root: &Path,
    text: &mut String,
    images: &mut Vec<Value>,
    path: &str,
) -> Result<(), String> {
    if path.len() > 4096 {
        return Err("human image path exceeds 4096 bytes".into());
    }
    if text.len().saturating_add(12) > MAX_TEXT {
        return Err("answer text has no room for an image marker".into());
    }
    if images.len() >= MAX_IMAGES {
        return Err("at most 16 answer images are supported".into());
    }
    let reference = crate::llm::images::user_attachment(root, Path::new(path))
        .map_err(|e| format!("cannot attach image: {e:#}"))?;
    images.push(reference);
    text.push_str(&format!("[Image #{}]", images.len()));
    Ok(())
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Correlation {
    pub question_id: String,
    pub generation: u64,
    pub session_id: String,
}
impl Correlation {
    pub fn matches(&self, request: &QuestionRequest) -> bool {
        self.question_id == request.question_id
            && self.generation == request.generation
            && self.session_id == request.session_id
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Submission {
    pub question_id: String,
    pub generation: u64,
    pub session_id: String,
    pub kind: String,
    #[serde(default)]
    pub results: Vec<SubmittedAnswer>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SubmittedAnswer {
    pub id: String,
    pub selected_options: Vec<String>,
    #[serde(default)]
    pub custom_input: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub custom_input_images: Vec<String>,
    #[serde(default)]
    pub note_images: Vec<String>,
}
impl Submission {
    pub fn correlation(&self) -> Correlation {
        Correlation {
            question_id: self.question_id.clone(),
            generation: self.generation,
            session_id: self.session_id.clone(),
        }
    }
    pub fn outcome(
        self,
        request: &QuestionRequest,
        root: &Path,
    ) -> Result<QuestionOutcome, String> {
        match self.kind.as_str() {
            "cancel" | "chat" if self.results.is_empty() => {
                return Ok(if self.kind == "cancel" {
                    QuestionOutcome::Cancelled
                } else {
                    QuestionOutcome::Chat
                });
            }
            "answers" => {}
            _ => {
                return Err(
                    "kind must be answers, chat or cancel; chat/cancel cannot contain results"
                        .into(),
                );
            }
        }
        if self.results.len() != request.questions.len() {
            return Err("answer count must match requested questions".into());
        }
        let count: usize = self
            .results
            .iter()
            .map(|answer| answer.custom_input_images.len() + answer.note_images.len())
            .sum();
        if count > MAX_IMAGES {
            return Err("answers support at most 16 image references".into());
        }
        let mut answers = Vec::with_capacity(self.results.len());
        let mut image_paths = Vec::with_capacity(self.results.len());
        for input in self.results {
            for path in input.custom_input_images.iter().chain(&input.note_images) {
                if path.len() > 4096 {
                    return Err("human image path exceeds 4096 bytes".into());
                }
            }
            let mut answer = QuestionAnswer {
                id: input.id,
                selected_options: input.selected_options,
                custom_input: input.custom_input,
                note: input.note,
                ..Default::default()
            };
            if !input.custom_input_images.is_empty() {
                answer.custom_input.get_or_insert_with(String::new);
            }
            if !input.note_images.is_empty() {
                answer.note.get_or_insert_with(String::new);
            }
            answers.push(answer);
            image_paths.push((input.custom_input_images, input.note_images));
        }
        // Reject malformed identities, choices and text before opening any human image path.
        validate_answers(request, &mut answers)?;
        for (answer, (custom, note)) in answers.iter_mut().zip(image_paths) {
            for (paths, text, images) in [
                (
                    custom,
                    &mut answer.custom_input,
                    &mut answer.custom_input_images,
                ),
                (note, &mut answer.note, &mut answer.note_images),
            ] {
                for path in paths {
                    attach(root, text.get_or_insert_with(String::new), images, &path)?;
                }
            }
        }
        validate_answers(request, &mut answers)?;
        Ok(QuestionOutcome::Answered(answers))
    }
}

/// Project genuine nested human input onto its enclosing tool message, not an invented tool round.
/// Each callback retains its local image markers and declares their offset in the enclosing list.
pub fn project_callbacks(message: &mut Value, mut records: Vec<Value>, text_limit: usize) {
    if records.is_empty() {
        return;
    }
    let mut images = message["dial_images"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let each_limit = (text_limit / records.len()).max(128);
    let mut summary = String::new();
    for (index, record) in records.iter_mut().enumerate() {
        let offset = images.len();
        record["image_offset"] = json!(offset);
        if let Some(references) = record["data"]["dial_images"].as_array() {
            images.extend(references.iter().cloned());
        }
        summary.push_str(&format!(
            "\n[Actual human Ask callback {} · local Image #N maps to enclosing Image #(N+{})]\n",
            index + 1,
            offset
        ));
        summary.push_str(&crate::agent::safety::guard::clip_middle(
            record["text"].as_str().unwrap_or(""),
            each_limit,
        ));
    }
    if let Some(content) = message["content"].as_str() {
        message["content"] = json!(format!("{content}{summary}"));
    }
    message["dial_question_results"] = json!(records);
    if !images.is_empty() {
        message["dial_images"] = json!(images);
    }
}
