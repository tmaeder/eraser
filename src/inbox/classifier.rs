//! Keyword-based classification of broker replies (port of `internal/inbox/classifier.go`).

use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

use super::parser::{
    ExtractedURLs, extract_bounced_recipient, get_primary_confirmation_url, get_primary_form_url,
    parse_email_urls,
};
use super::{Email, go_lower, go_regex};

/// The type of broker response. String values match the Go/DB representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseType {
    /// Need to fill out a form
    FormRequired,
    /// Need to click confirmation link
    ConfirmationRequired,
    /// Removal confirmed/completed
    Success,
    /// Request denied
    Rejected,
    /// Processing, will follow up
    Pending,
    /// Email bounced - invalid address
    Bounced,
    /// Needs manual review
    Unknown,
}

impl ResponseType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FormRequired => "form_required",
            Self::ConfirmationRequired => "confirmation_required",
            Self::Success => "success",
            Self::Rejected => "rejected",
            Self::Pending => "pending",
            Self::Bounced => "bounced",
            Self::Unknown => "unknown",
        }
    }
}

impl std::fmt::Display for ResponseType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A classified email response.
#[derive(Debug, Clone, PartialEq)]
pub struct ClassifiedResponse {
    pub email: Email,
    pub r#type: ResponseType,
    pub urls: ExtractedURLs,
    /// Primary form URL (if form_required)
    pub form_url: String,
    /// Primary confirmation URL (if confirmation_required)
    pub confirm_url: String,
    /// Address that bounced (if bounced)
    pub bounced_recipient: String,
    pub confidence: f64,
    /// Human-readable reason for classification
    pub reason: String,
    /// Whether manual review is recommended
    pub needs_review: bool,
}

fn compile(patterns: &[&str]) -> Vec<Regex> {
    patterns.iter().map(|p| go_regex(p)).collect()
}

static SUCCESS_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    compile(&[
        r"(?i)request\s+(has\s+been\s+)?(completed|processed|fulfilled)",
        r"(?i)successfully\s+(removed|deleted|opted\s*out)",
        r"(?i)your\s+(data|information)\s+(has\s+been\s+)?(removed|deleted)",
        r"(?i)opt[\s-]?out\s+(request\s+)?(is\s+)?(complete|confirmed)",
        r"(?i)we\s+have\s+(removed|deleted)",
        r"(?i)no\s+longer\s+(have|hold|store)\s+your\s+(data|information)",
    ])
});

static FORM_REQUIRED_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    compile(&[
        r"(?i)please\s+(complete|fill\s*(out|in)?|submit)\s+(the|our|this)?\s*(form|request)",
        r"(?i)visit\s+(the\s+)?(following\s+)?(link|url|page)\s+to\s+(complete|submit|verify)",
        r"(?i)click\s+(here|below|the\s+link)\s+to\s+(begin|start|submit|complete)",
        r"(?i)(must|need\s+to)\s+(verify|confirm)\s+your\s+(identity|request)",
        r"(?i)submit\s+a\s+(formal\s+)?request\s+(through|via|at)",
        r"(?i)use\s+(our|the)\s+(online|web)\s*(form|portal|tool)",
        // Redirect to form patterns (from real broker emails)
        r"(?i)please\s+submit\s+(your\s+)?request\s+(at|via|through)\s+",
        r"(?i)please\s+use\s+(our|the)\s+(opt[\s-]?out|removal|privacy)\s*(form|page|link)",
        r"(?i)complete\s+(the|our|a)\s+(data\s+subject|privacy|opt[\s-]?out)\s*(access\s+)?(request\s+)?form",
        r"(?i)submit\s+(a\s+|your\s+)?(request|form)\s+(via|through|at|using)\s+(our\s+)?(online|web|interactive)",
        r"(?i)(does\s+not|do\s+not|cannot)\s+accept\s+privacy\s+requests?\s+(via|by|through)\s+email",
        r"(?i)this\s+email\s+(address\s+)?is\s+not\s+intended\s+for\s+privacy",
        r"(?i)visit\s+(our|the)\s+(opt[\s-]?out|removal|privacy)\s*(page|form|portal)",
        r"(?i)(data\s+subject|privacy)\s+requests?\s+(can|should|must)\s+be\s+(filed|submitted)\s+(at|via)",
        r"(?i)go\s+to\s+(the\s+)?(link|url|page)\s+(below|above)",
        r"(?i)please\s+click\s+(on\s+)?(the\s+)?following\s+link",
        r"(?i)you\s+(can|may)\s+submit\s+.{0,30}(privacy|opt[\s-]?out)",
        r"(?i)right\s+to\s+(opt[\s-]?out|delete|know)[:\s]",
        // Additional form patterns from real emails
        r"(?i)we\s+(have\s+)?established\s+a\s+dedicated\s+(online\s+)?form",
        r"(?i)we\s+do\s+not\s+process\s+requests?\s+via\s+email",
        r"(?i)please\s+send\s+(your?\s+)?request\s+to\s+customer\s+service",
        r"(?i)is\s+not\s+(a\s+)?mechanism\s+for.{0,30}(privacy|request)",
        r"(?i)please\s+complete\s+your\s+(request|form)",
    ])
});

/// Need to click a link to verify identity/email.
static CONFIRMATION_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    compile(&[
        r"(?i)click\s+(here|below|the\s+link)\s+to\s+(confirm|verify|validate)",
        r"(?i)please\s+(confirm|verify)\s+(your\s+)?(email|request|identity)",
        r"(?i)verification\s+(link|email|code)",
        r"(?i)confirm\s+your\s+(email\s+)?(address)?",
        r"(?i)click\s+(to\s+)?confirm",
        // Verification of identity patterns
        r"(?i)(can|could)\s+you\s+(please\s+)?verify",
        r"(?i)verify\s+(your\s+)?(last\s+4|ssn|social)",
    ])
});

static REJECTION_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    compile(&[
        r"(?i)(cannot|can't|unable\s+to)\s+(process|complete|fulfill)\s+(your\s+)?request",
        r"(?i)request\s+(has\s+been\s+)?(denied|rejected|declined)",
        r"(?i)do\s+not\s+have\s+(any\s+)?(data|information|records)\s+(about|for|on)\s+you",
        r"(?i)not\s+found\s+in\s+our\s+(system|database|records)",
        r"(?i)no\s+(matching\s+)?(records?|data|information)\s+found",
        r"(?i)exempt\s+from\s+(CCPA|GDPR|this\s+request)",
        // No data found patterns (from real broker emails)
        r"(?i)(do\s+not|don't)\s+have\s+(any\s+)?record",
        r"(?i)no\s+(matching\s+)?record\s+(of\s+)?(a\s+)?report",
        r"(?i)maintains?\s+no\s+(files?|records?|data)",
        // Service discontinued patterns
        r"(?i)no\s+longer\s+(registered|operating)\s+(as\s+)?(an?\s+)?(active\s+)?data\s+broker",
        r"(?i)(this\s+)?(email|inbox)(\s+\w+)?\s+(is\s+)?(no\s+longer|being\s+retired)",
        r"(?i)service\s+offerings?\s+no\s+longer\s+include",
        // Additional rejection patterns from real emails
        r"(?i)(we\s+)?(have\s+)?no\s+data\s+(linked|associated|related)\s+to\s+(your|this)",
        r"(?i)not\s+identified\s+in\s+our\s+database",
        r"(?i)(we\s+are|we're)\s+a\s+b2b\s+(platform|company|business)",
        r"(?i)has\s+never\s+existed\s+in\s+our\s+database",
        r"(?i)consumer\s+reporting\s+agenc(y|ies)\s+(is|are)\s+exempt",
        r"(?i)fair\s+credit\s+reporting\s+act.{0,30}exempt",
        r"(?i)we\s+do\s+not\s+(remove|delete)\s+data\s+by\s+request",
        r"(?i)(your\s+)?(email|name|address|information)\s+was\s+not\s+identified",
        // Wrong email address patterns
        r"(?i)(this\s+)?(email|inbox)\s+(address\s+)?(is\s+)?not\s+(a\s+)?(mechanism|intended)\s+(for|to)",
        r"(?i)not\s+intended\s+for\s+(the\s+)?(submission|handling)\s+of\s+privacy",
        r"(?i)will\s+not\s+be\s+considered\s+a\s+valid\s+submission",
    ])
});

static PENDING_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    compile(&[
        r"(?i)(is\s+being|currently\s+being)\s+(processed|reviewed|handled)",
        r"(?i)will\s+(process|complete|handle)\s+(your\s+)?request\s+within",
        r"(?i)please\s+allow\s+(\d+)\s+(days|business\s+days|weeks)",
        r"(?i)we('ll|\s+will)\s+(get\s+back|respond|follow\s+up)",
        r"(?i)request\s+(has\s+been\s+)?(received|acknowledged)",
        // Request acknowledgment patterns (from real broker emails)
        r"(?i)thank\s+you\s+for\s+(your\s+)?(inquiry|email|contacting|reaching|privacy)",
        r"(?i)we\s+(have\s+)?received\s+your\s+(request|email|inquiry)",
        r"(?i)(has\s+been\s+)?assigned\s+(a\s+)?(ticket|case|reference)\s*(number|#|id)?",
        r"(?i)one\s+of\s+our\s+.{0,30}(will\s+)?(reach\s+out|respond|contact)",
        r"(?i)ticket\s+(has\s+been\s+)?(created|opened|received)",
        // Additional patterns for better subject line matching
        r"(?i)your\s+request\s+has\s+been\s+received",
        r"(?i)support\s+request\s*#?\d+",
        r"(?i)legal\s+request\s+received",
        r"(?i)i\s+(have\s+)?(now\s+)?left\s+", // Person left the company
        r"(?i)no\s+longer\s+with\s+(the\s+)?(company|organization)",
        // Additional pending patterns from real emails
        r"(?i)(will\s+be\s+)?(removed|deleted)\s+from\s+our\s+database.{0,20}\d+\s+days",
        r"(?i)once\s+verified.{0,30}(will\s+be\s+)?(processed|complete)",
        r"(?i)this\s+(message\s+)?confirms\s+(our\s+)?receipt",
        r"(?i)we\s+appreciate\s+your\s+interest\s+in\s+exercising",
        r"(?i)request\s+(will\s+be\s+)?(processed|fulfilled)",
        r"(?i)automatic\s+reply",
        r"(?i)auto[\s-]?response",
    ])
});

/// Subject-only pending patterns (stronger signal, +3).
static SUBJECT_PENDING_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    compile(&[
        r"(?i)^automatic\s+reply",
        r"(?i)^auto[\s-]?reply",
        r"(?i)^auto[\s-]?response",
        r"(?i)^out\s+of\s+office",
        r"(?i)request\s+received",
        r"(?i)has\s+been\s+received",
        r"(?i)thank\s+you\s+for\s+your\s+(privacy|data|removal|email)",
        r"(?i)thank\s+you\s+for\s+(your\s+)?email\s+to", // "Thank you for your email to Nielsen's Privacy Team"
        r"(?i)thanks\s+for\s+(reaching|contacting)",
        r"(?i)#[A-Z]{0,3}[-]?\d{5,}", // Ticket numbers like #REQ-195698, #LD00019726
        r"(?i)request\s*#\s*\d+",
        r"(?i)support\s+request",
        r"(?i)ticket\s*[\(#]\s*:?\s*\d+", // Ticket (259135) or Ticket #259135 or Ticket #: 259135
        r"(?i)we\s+have\s+received\s+your\s+ticket",
        r"(?i)i\s+(have\s+)?(now\s+)?left\s+", // Person left the company
        r"(?i)no\s+longer\s+with\s+(the\s+)?(company|organization)",
        r"(?i)office\s+closed",
        r"(?i)response\s+to\s+your\s+email",
    ])
});

/// Test emails sent by Eraser itself.
static TEST_EMAIL_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    compile(&[
        r"(?i)eraser\s+test\s+email",
        r"(?i)test\s+email\s+from\s+eraser",
        r"(?i)this\s+is\s+a\s+test\s+email",
    ])
});

static SUBJECT_REJECTION_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    compile(&[
        r"(?i)not\s+found",
        r"(?i)no\s+record",
        r"(?i)unable\s+to\s+(locate|find|process)",
        r"(?i)request\s+(denied|rejected)",
    ])
});

static SUBJECT_SUCCESS_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    compile(&[
        r"(?i)opt[\s-]?out\s+(has\s+been\s+)?completed",
        r"(?i)(has\s+been\s+|successfully\s+)?(removed|deleted)",
        r"(?i)ticket.+solved",
        r"(?i)request\s+(has\s+been\s+)?(completed|processed|fulfilled)",
    ])
});

static SUBJECT_FORM_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    compile(&[
        r"(?i)opt[\s-]?out\s+instructions",
        r"(?i)removal\s+instructions",
        r"(?i)how\s+to\s+(opt[\s-]?out|remove)",
    ])
});

static BOUNCE_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    compile(&[
        r"(?i)delivery\s+(to\s+.+\s+)?(has\s+)?failed",
        r"(?i)undeliverable",
        r"(?i)delivery\s+status\s+notification",
        r"(?i)returned\s+mail",
        r"(?i)mail\s+delivery\s+failed",
        r"(?i)message\s+(could\s+)?not\s+(be\s+)?delivered",
        r"(?i)could\s+not\s+be\s+delivered",
        r"(?i)delivery\s+failure",
        r"(?i)permanent\s+(failure|error)",
        r"(?i)address\s+rejected",
        r"(?i)user\s+unknown",
        r"(?i)mailbox\s+not\s+found",
        r"(?i)no\s+such\s+user",
        r"(?i)(mailbox|recipient|address)\s+(does\s+not|doesn't)\s+exist",
        r"(?i)invalid\s+(recipient|address|mailbox)",
        r"(?i)unknown\s+(recipient|user|address)",
        r"(?i)550\s+.*\s+(rejected|unknown|not\s+found)",
        r"(?i)554\s+.*\s+(rejected|failed)",
    ])
});

/// Senders that indicate a bounce email.
const BOUNCE_SENDERS: &[&str] = &[
    "mailer-daemon",
    "postmaster",
    "mail delivery system",
    "mail delivery subsystem",
    "mailerdaemon",
    "noreply",
    "no-reply",
    "mailsystem",
];

/// Score slots in fixed order. Go iterated a map here, so ties were broken
/// randomly; we break them deterministically in this (Go declaration) order.
const SCORED_TYPES: [ResponseType; 5] = [
    ResponseType::Success,
    ResponseType::FormRequired,
    ResponseType::ConfirmationRequired,
    ResponseType::Rejected,
    ResponseType::Pending,
];
const SUCCESS: usize = 0;
const FORM: usize = 1;
const CONFIRM: usize = 2;
const REJECTED: usize = 3;
const PENDING: usize = 4;

fn count(patterns: &[Regex], text: &str) -> i32 {
    patterns.iter().filter(|p| p.is_match(text)).count() as i32
}

fn count_either(patterns: &[Regex], a: &str, b: &str) -> i32 {
    patterns
        .iter()
        .filter(|p| p.is_match(a) || p.is_match(b))
        .count() as i32
}

/// Subject-specific patterns, +3 per match.
fn add_subject_scores(scores: &mut [i32; 5], subject: &str) {
    scores[PENDING] += 3 * count(&SUBJECT_PENDING_PATTERNS, subject);
    scores[REJECTED] += 3 * count(&SUBJECT_REJECTION_PATTERNS, subject);
    scores[SUCCESS] += 3 * count(&SUBJECT_SUCCESS_PATTERNS, subject);
    scores[FORM] += 3 * count(&SUBJECT_FORM_PATTERNS, subject);
}

/// Analyzes an email and determines the response type.
pub fn classify_response(email: &Email) -> ClassifiedResponse {
    let mut result = ClassifiedResponse {
        email: email.clone(),
        r#type: ResponseType::Unknown,
        urls: parse_email_urls(email),
        form_url: String::new(),
        confirm_url: String::new(),
        bounced_recipient: String::new(),
        confidence: 0.0,
        reason: String::new(),
        needs_review: false,
    };

    let content = go_lower(&if email.body.is_empty() {
        strip_html(&email.html_body)
    } else {
        email.body.clone()
    });
    let subject = go_lower(&email.subject);

    if is_test_email(&subject, &content) {
        result.r#type = ResponseType::Success;
        result.confidence = 1.0;
        result.reason = "Test email from Eraser - email configuration verified".into();
        return result;
    }

    // Bounces are clear-cut; check them before anything else.
    if is_bounce_email(email, &subject, &content) {
        result.r#type = ResponseType::Bounced;
        result.confidence = 0.95;
        result.reason = "Email delivery failed - address may be invalid".into();
        result.bounced_recipient = extract_bounced_recipient(email);
        return result;
    }

    let mut scores = [0i32; 5];
    add_subject_scores(&mut scores, &subject);
    scores[SUCCESS] += count_either(&SUCCESS_PATTERNS, &content, &subject);
    scores[FORM] += count(&FORM_REQUIRED_PATTERNS, &content);
    scores[CONFIRM] += count(&CONFIRMATION_PATTERNS, &content);
    scores[REJECTED] += count_either(&REJECTION_PATTERNS, &content, &subject);
    scores[PENDING] += count(&PENDING_PATTERNS, &content);

    if !result.urls.form_urls.is_empty() {
        scores[FORM] += 2;
    }
    if !result.urls.confirmation_urls.is_empty() {
        scores[CONFIRM] += 2;
    }

    let mut max_score = 0;
    let mut second_score = 0;
    for (i, &score) in scores.iter().enumerate() {
        if score > max_score {
            second_score = max_score;
            max_score = score;
            result.r#type = SCORED_TYPES[i];
        } else if score > second_score {
            second_score = score;
        }
    }

    if max_score > 0 {
        result.confidence = if second_score == 0 {
            0.85 // Only one type matched
        } else {
            let margin = f64::from(max_score - second_score) / f64::from(max_score);
            0.5 + margin * 0.4 // Range: 0.5 to 0.9
        };
        if max_score >= 3 {
            result.confidence = result.confidence.max(0.75);
        }
        // Concrete URLs back up the classification
        if (result.r#type == ResponseType::FormRequired && !result.urls.form_urls.is_empty())
            || (result.r#type == ResponseType::ConfirmationRequired
                && !result.urls.confirmation_urls.is_empty())
        {
            result.confidence = result.confidence.max(0.85);
        }
    } else if !result.urls.confirmation_urls.is_empty() {
        result.r#type = ResponseType::ConfirmationRequired;
        result.confidence = 0.5;
    } else if !result.urls.form_urls.is_empty() {
        result.r#type = ResponseType::FormRequired;
        result.confidence = 0.5;
    }

    result.form_url = get_primary_form_url(&result.urls, &email.from_domain);
    result.confirm_url = get_primary_confirmation_url(&result.urls, &email.from_domain);
    result.reason = classification_reason(result.r#type).into();
    result.needs_review = result.r#type == ResponseType::Unknown || result.confidence < 0.4;
    result
}

fn classification_reason(response_type: ResponseType) -> &'static str {
    match response_type {
        ResponseType::Success => "Email indicates removal request was completed successfully",
        ResponseType::FormRequired => "Email contains link to opt-out form that needs to be filled",
        ResponseType::ConfirmationRequired => {
            "Email contains confirmation link that needs to be clicked"
        }
        ResponseType::Rejected => "Broker rejected or could not process the removal request",
        ResponseType::Pending => "Request is being processed, follow-up may be needed",
        ResponseType::Unknown => "Could not automatically classify this response",
        ResponseType::Bounced => "Unknown classification",
    }
}

static SCRIPT_RE: LazyLock<Regex> = LazyLock::new(|| go_regex(r"(?is)<script[^>]*>.*?</script>"));
static STYLE_RE: LazyLock<Regex> = LazyLock::new(|| go_regex(r"(?is)<style[^>]*>.*?</style>"));
static TAG_RE: LazyLock<Regex> = LazyLock::new(|| go_regex(r"<[^>]+>"));
static WHITESPACE_RE: LazyLock<Regex> = LazyLock::new(|| go_regex(r"\s+"));

/// Simple HTML to text: drop script/style, tags to spaces, decode a few entities, collapse whitespace.
pub(crate) fn strip_html(html: &str) -> String {
    let s = SCRIPT_RE.replace_all(html, "");
    let s = STYLE_RE.replace_all(&s, "");
    let s = TAG_RE
        .replace_all(&s, " ")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"");
    WHITESPACE_RE.replace_all(&s, " ").trim().to_string()
}

pub fn classify_batch(emails: &[Email]) -> Vec<ClassifiedResponse> {
    emails.iter().map(classify_response).collect()
}

/// Classifies from the subject line only (for reclassifying stored records).
/// Returns (type, confidence, needs_review).
pub fn classify_by_subject_only(subject: &str) -> (ResponseType, f64, bool) {
    let subject = go_lower(subject);
    let mut scores = [0i32; 5];
    add_subject_scores(&mut scores, &subject);
    scores[SUCCESS] += count(&SUCCESS_PATTERNS, &subject);
    scores[REJECTED] += count(&REJECTION_PATTERNS, &subject);
    scores[PENDING] += count(&PENDING_PATTERNS, &subject);

    let mut max_score = 0;
    let mut best = ResponseType::Unknown;
    for (i, &score) in scores.iter().enumerate() {
        if score > max_score {
            max_score = score;
            best = SCORED_TYPES[i];
        }
    }

    match max_score {
        3.. => (best, 0.7, false), // Strong subject match
        1.. => (best, 0.4, true),  // Weak match
        _ => (ResponseType::Unknown, 0.0, true),
    }
}

pub fn filter_by_type(
    responses: &[ClassifiedResponse],
    response_type: ResponseType,
) -> Vec<ClassifiedResponse> {
    responses
        .iter()
        .filter(|r| r.r#type == response_type)
        .cloned()
        .collect()
}

/// Responses that need action (form or confirmation).
pub fn get_actionable_responses(responses: &[ClassifiedResponse]) -> Vec<ClassifiedResponse> {
    responses
        .iter()
        .filter(|r| {
            matches!(
                r.r#type,
                ResponseType::FormRequired | ResponseType::ConfirmationRequired
            )
        })
        .cloned()
        .collect()
}

pub fn get_bounced_responses(responses: &[ClassifiedResponse]) -> Vec<ClassifiedResponse> {
    filter_by_type(responses, ResponseType::Bounced)
}

/// Counts of classified responses by type.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Summary {
    pub total: usize,
    pub success: usize,
    pub form_required: usize,
    pub confirm_required: usize,
    pub rejected: usize,
    pub pending: usize,
    pub bounced: usize,
    pub unknown: usize,
    pub need_review: usize,
}

pub fn summarize_responses(responses: &[ClassifiedResponse]) -> Summary {
    let mut s = Summary {
        total: responses.len(),
        ..Default::default()
    };
    for r in responses {
        *match r.r#type {
            ResponseType::Success => &mut s.success,
            ResponseType::FormRequired => &mut s.form_required,
            ResponseType::ConfirmationRequired => &mut s.confirm_required,
            ResponseType::Rejected => &mut s.rejected,
            ResponseType::Pending => &mut s.pending,
            ResponseType::Bounced => &mut s.bounced,
            ResponseType::Unknown => &mut s.unknown,
        } += 1;
        if r.needs_review {
            s.need_review += 1;
        }
    }
    s
}

fn is_test_email(subject: &str, content: &str) -> bool {
    TEST_EMAIL_PATTERNS
        .iter()
        .any(|p| p.is_match(subject) || p.is_match(content))
}

/// From a mail-system sender with any bounce pattern, or a strong bounce score (>= 3).
fn is_bounce_email(email: &Email, subject: &str, content: &str) -> bool {
    let from = go_lower(&email.from);
    let from_name = go_lower(&email.from_name);
    let is_bounce_source = BOUNCE_SENDERS
        .iter()
        .any(|s| from.contains(s) || from_name.contains(s));
    let bounce_score = 2 * count(&BOUNCE_PATTERNS, subject) + count(&BOUNCE_PATTERNS, content);
    (is_bounce_source && bounce_score > 0) || bounce_score >= 3
}

#[cfg(test)]
mod tests {
    use super::*;
    use ResponseType::*;

    const DEFAULT_SUBJECT: &str = "Re: Personal Data Removal Request";

    fn classify(subject: &str, body: &str) -> ClassifiedResponse {
        classify_response(&Email {
            subject: subject.into(),
            body: body.into(),
            ..Default::default()
        })
    }

    /// (name, subject, body, expected)
    fn check(cases: &[(&str, &str, &str, ResponseType)]) {
        for &(name, subject, body, expected) in cases {
            let r = classify(subject, body);
            assert_eq!(
                r.r#type, expected,
                "{name}: got {} (confidence {:.2})",
                r.r#type, r.confidence
            );
        }
    }

    #[test]
    fn form_required_patterns() {
        let s = DEFAULT_SUBJECT;
        check(&[
            (
                "Blackbaud - submit request at URL",
                s,
                "please submit your request at https://www.blackbaud.com/company/Data-Subject-Rights-Request",
                FormRequired,
            ),
            (
                "FamilyTreeNow - use opt-out form",
                s,
                "please use our Opt-Out Form, linked below, to submit your request",
                FormRequired,
            ),
            (
                "Civis - complete data subject request form",
                s,
                "please complete the Data Subject Request Form, which you can find at",
                FormRequired,
            ),
            (
                "Affinity - does not accept privacy requests via email",
                s,
                "does not accept privacy requests via email. You can submit a privacy request via our web form",
                FormRequired,
            ),
            (
                "33Across - visit opt-out page",
                s,
                "Please visit our opt-out page located at https://www.33across.com/opt-out",
                FormRequired,
            ),
            (
                "PeopleFinders - right to opt-out",
                s,
                "Right to Opt-out: If you wish to opt out of our website, you may do so at this link",
                FormRequired,
            ),
            (
                "01Advertising - click following link",
                s,
                "please click on the following link to submit your data deletion request",
                FormRequired,
            ),
            (
                "Mediaocean - dedicated online form",
                s,
                "we have established a dedicated online form that helps us verify your identity",
                FormRequired,
            ),
            (
                "Do not process via email",
                s,
                "We do not process requests via email alone. Please submit via our web form.",
                FormRequired,
            ),
            (
                "Send to customer service",
                s,
                "Dear Customer, Please send your request to Customer Service at custserv@example.com",
                FormRequired,
            ),
        ]);
    }

    #[test]
    fn pending_patterns() {
        check(&[
            (
                "BeenVerified - received your request",
                "Your Request Has Been Received",
                "we have received your request. One of our Privacy Specialists will reach out",
                Pending,
            ),
            (
                "6sense - ticket assigned",
                "Request Received - [#REQ-195698]",
                "ticket has been created. Please reference #REQ-195698",
                Pending,
            ),
            (
                "Automatic reply in subject",
                "Automatic reply: Personal Data Removal Request",
                "Thank you for contacting us.",
                Pending,
            ),
            (
                "Out of office",
                "Out of Office Re: Personal Data Removal Request",
                "I am currently out of the office",
                Pending,
            ),
            (
                "Thank you for your inquiry",
                DEFAULT_SUBJECT,
                "Thank you for your inquiry. This email confirms that we have received your request.",
                Pending,
            ),
        ]);
    }

    #[test]
    fn confirmation_patterns() {
        let s = DEFAULT_SUBJECT;
        check(&[
            (
                "Click here to confirm",
                s,
                "Please click here to confirm your request",
                ConfirmationRequired,
            ),
            (
                "Click below to verify",
                s,
                "Click below to verify your email address",
                ConfirmationRequired,
            ),
            (
                "Please confirm your email",
                s,
                "Please confirm your email address to process your request",
                ConfirmationRequired,
            ),
            (
                "Verification link",
                s,
                "We have sent you a verification link. Please click it to continue.",
                ConfirmationRequired,
            ),
            (
                "Click to confirm",
                s,
                "Click to confirm your data removal request",
                ConfirmationRequired,
            ),
            (
                "Please verify your identity",
                s,
                "For security purposes, please verify your identity by clicking the link below",
                ConfirmationRequired,
            ),
            (
                "Confirm your request",
                s,
                "Click the link to confirm your request: https://example.com/confirm/abc123",
                ConfirmationRequired,
            ),
            (
                "Verify SSN last 4",
                s,
                "Can you please verify last 4 of your social? We have several individuals with your name.",
                ConfirmationRequired,
            ),
        ]);
    }

    #[test]
    fn rejection_patterns() {
        let s = DEFAULT_SUBJECT;
        check(&[
            (
                "ACUTRAQ - no record",
                s,
                "we do not have any record of a report in our system. ACUTRAQ maintains no files on this person.",
                Rejected,
            ),
            (
                "Atlantic Fox - no longer a data broker",
                s,
                "Atlantic Fox is no longer registered as an active data broker in any jurisdictions",
                Rejected,
            ),
            (
                "Checkr - email no longer in use",
                s,
                "The privacy@checkr.com email list is no longer in use",
                Rejected,
            ),
            (
                "No data linked to email",
                s,
                "We have no data linked to your email in our system.",
                Rejected,
            ),
            (
                "B2B platform",
                s,
                "Tyler - we are a b2b platform and do not have your information.",
                Rejected,
            ),
            (
                "Never existed in database",
                s,
                "the information below has never existed in our database.",
                Rejected,
            ),
            (
                "FCRA exempt",
                s,
                "consumer reporting agencies are exempt as Fair Credit Reporting Act related data so we do not remove data by request.",
                Rejected,
            ),
            (
                "Not identified in database",
                s,
                "Your email was not identified in our database. This means that we don't have any personal information.",
                Rejected,
            ),
        ]);
    }

    #[test]
    fn subject_based_pending_patterns() {
        check(&[
            (
                "Your Request Has Been Received",
                "Your Request Has Been Received",
                "",
                Pending,
            ),
            (
                "Thank you for your Privacy Request",
                "Thank you for your Privacy Request",
                "",
                Pending,
            ),
            (
                "Ticket number in subject - REQ format",
                "Request Received - [#REQ-195698] Personal Data Removal Request",
                "",
                Pending,
            ),
            (
                "Ticket number in subject - LD format",
                "#LD00019726 Personal Data Removal Request Legal Request Received",
                "",
                Pending,
            ),
            (
                "Support Request with number",
                "Your Convex Support Request #317855",
                "",
                Pending,
            ),
            (
                "Auto-reply with minimal body",
                "Automatic reply: Personal Data Removal Request",
                "Thank you.",
                Pending,
            ),
            (
                "Auto response variant",
                "Auto-Response: Personal Data Removal Request",
                "",
                Pending,
            ),
            (
                "Out of office with minimal body",
                "Out of Office Re: Personal Data Removal Request",
                "",
                Pending,
            ),
            (
                "I have left the company",
                "I have now left Captify Re: Personal Data Removal Request",
                "",
                Pending,
            ),
        ]);
    }

    #[test]
    fn subject_based_rejection_patterns() {
        check(&[
            (
                "Not Found in subject",
                "[Convex] Re: Deletion Request Update - Not Found",
                "",
                Rejected,
            ),
            (
                "No record in subject",
                "Re: Personal Data Removal Request - No Record Found",
                "",
                Rejected,
            ),
            (
                "Unable to locate",
                "Unable to locate your data - Personal Data Removal Request",
                "",
                Rejected,
            ),
        ]);
    }

    #[test]
    fn test_email_patterns() {
        let cases = [
            ("Eraser Test Email in subject", "Eraser Test Email", ""),
            (
                "Test email in body",
                "Re: Test",
                "This is a test email from Eraser to verify your email configuration.",
            ),
        ];
        for (name, subject, body) in cases {
            let r = classify(subject, body);
            assert_eq!(r.r#type, Success, "{name}: confidence {:.2}", r.confidence);
            assert!(!r.needs_review, "{name}: test email should not need review");
        }
    }

    #[test]
    fn ticket_patterns() {
        check(&[
            (
                "Ticket #: format",
                "Ticket #: 259135 Re: Personal Data Removal Request",
                "",
                Pending,
            ),
            (
                "Thank you for email to privacy team",
                "Thank you for your email to Nielsen's Privacy Team Re: Personal Data Removal Request",
                "",
                Pending,
            ),
        ]);
    }

    #[test]
    fn response_type_strings() {
        assert_eq!(FormRequired.as_str(), "form_required");
        assert_eq!(
            serde_json::to_string(&ConfirmationRequired).unwrap(),
            "\"confirmation_required\""
        );
    }

    /// Differential test against the Go implementation: tests/fixtures/inbox/golden.json
    /// was produced by running the original Go code over corpus.json (Go's random
    /// tie-breaking replaced by the same fixed order used here; `go_types` records
    /// every type unmodified Go can return).
    mod differential {
        use super::super::*;
        use crate::inbox::parser::{clean_url, extract_confirmation_token};
        use serde::Deserialize;

        #[derive(Deserialize)]
        struct Corpus {
            emails: Vec<CorpusEmail>,
        }

        #[derive(Deserialize)]
        struct CorpusEmail {
            name: String,
            from: String,
            from_name: String,
            from_domain: String,
            subject: String,
            body: String,
            html_body: String,
        }

        #[derive(Deserialize, Debug, PartialEq)]
        struct Urls {
            form: Vec<String>,
            confirmation: Vec<String>,
            unsubscribe: Vec<String>,
            all: Vec<String>,
        }

        #[derive(Deserialize, Debug, PartialEq)]
        struct EmailResult {
            name: String,
            r#type: ResponseType,
            confidence: f64,
            reason: String,
            needs_review: bool,
            form_url: String,
            confirm_url: String,
            bounced_recipient: String,
            urls: Urls,
            extract_bounced: String,
            tokens: Vec<String>,
            stripped_html: String,
            go_types: Vec<ResponseType>,
        }

        #[derive(Deserialize, Debug, PartialEq)]
        struct UrlResult {
            input: String,
            clean: String,
            token: String,
        }

        #[derive(Deserialize, Debug, PartialEq)]
        struct SubjectResult {
            subject: String,
            r#type: ResponseType,
            confidence: f64,
            needs_review: bool,
            go_types: Vec<ResponseType>,
        }

        #[derive(Deserialize)]
        struct Golden {
            emails: Vec<EmailResult>,
            urls: Vec<UrlResult>,
            subjects: Vec<SubjectResult>,
        }

        #[test]
        fn matches_go_golden() {
            let corpus: Corpus =
                serde_json::from_str(include_str!("../../tests/fixtures/inbox/corpus.json"))
                    .unwrap();
            let golden: Golden =
                serde_json::from_str(include_str!("../../tests/fixtures/inbox/golden.json"))
                    .unwrap();
            assert_eq!(corpus.emails.len(), golden.emails.len());

            for (ce, want) in corpus.emails.iter().zip(&golden.emails) {
                let email = Email {
                    from: ce.from.clone(),
                    from_name: ce.from_name.clone(),
                    from_domain: ce.from_domain.clone(),
                    subject: ce.subject.clone(),
                    body: ce.body.clone(),
                    html_body: ce.html_body.clone(),
                    ..Default::default()
                };
                let r = classify_response(&email);
                let got = EmailResult {
                    name: ce.name.clone(),
                    r#type: r.r#type,
                    confidence: r.confidence,
                    reason: r.reason,
                    needs_review: r.needs_review,
                    form_url: r.form_url,
                    confirm_url: r.confirm_url,
                    bounced_recipient: r.bounced_recipient,
                    tokens: r
                        .urls
                        .confirmation_urls
                        .iter()
                        .map(|u| extract_confirmation_token(u))
                        .collect(),
                    urls: Urls {
                        form: r.urls.form_urls,
                        confirmation: r.urls.confirmation_urls,
                        unsubscribe: r.urls.unsubscribe_urls,
                        all: r.urls.all_urls,
                    },
                    extract_bounced: crate::inbox::parser::extract_bounced_recipient(&email),
                    stripped_html: strip_html(&ce.html_body),
                    go_types: want.go_types.clone(),
                };
                assert!(
                    want.go_types.contains(&want.r#type),
                    "{}: golden inconsistent",
                    ce.name
                );
                assert_eq!(&got, want, "email case {:?}", ce.name);
            }

            for want in &golden.urls {
                let got = UrlResult {
                    input: want.input.clone(),
                    clean: clean_url(&want.input),
                    token: extract_confirmation_token(&want.input),
                };
                assert_eq!(&got, want);
            }

            for want in &golden.subjects {
                let (t, confidence, needs_review) = classify_by_subject_only(&want.subject);
                let got = SubjectResult {
                    subject: want.subject.clone(),
                    r#type: t,
                    confidence,
                    needs_review,
                    go_types: want.go_types.clone(),
                };
                assert_eq!(&got, want);
            }
        }
    }
}
