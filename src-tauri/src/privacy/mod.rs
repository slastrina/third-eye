//! Deterministic on-device redaction engine (D028).
//!
//! Pure, offline, deterministic, panic-free: regex + Luhn + entropy/context
//! rules only — no LLM, no network, no randomness. Mounted at the watcher
//! observation fan-out (D029 mount 1) so every `TextObservation` consumer
//! receives already-redacted text.
//!
//! Contract pinned by unit tests below:
//! - Placeholder vocabulary is byte-exact: `[REDACTED:password]`,
//!   `[REDACTED:card]`, `[REDACTED:api-key]`.
//! - `Detection` serializes with camelCase fields and kebab-case kind tags
//!   (the shape S03 counters surface).
//! - Detections carry kinds and counts only — never original text.

use std::sync::LazyLock;

use regex::Regex;
use serde::Serialize;

/// Inputs larger than this still get fully scanned (the regex engine is
/// linear-time), but the outcome is flagged [`RedactionConfidence::Low`]:
/// screen-sized observations never approach this, so an oversized input is
/// itself an anomaly S02's fail-closed policy may act on.
pub const SCAN_CAP_BYTES: usize = 64 * 1024;

/// Minimum length for a generic (non-prefixed) secret-token candidate.
const GENERIC_TOKEN_MIN_LEN: usize = 20;

/// Shannon-entropy floor (bits per char) for generic token candidates.
const GENERIC_TOKEN_MIN_ENTROPY: f64 = 3.5;

/// What kind of sensitive value a detector matched. Kebab-case kind tags
/// match every other kind tag in the app; S03 counters key off these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DetectionKind {
    Password,
    Card,
    ApiKey,
}

impl DetectionKind {
    /// Stable detection/report order (also the internal counter index).
    pub const ALL: [DetectionKind; 3] = [
        DetectionKind::Password,
        DetectionKind::Card,
        DetectionKind::ApiKey,
    ];

    /// The byte-exact placeholder substituted into redacted text.
    pub fn placeholder(self) -> &'static str {
        match self {
            DetectionKind::Password => "[REDACTED:password]",
            DetectionKind::Card => "[REDACTED:card]",
            DetectionKind::ApiKey => "[REDACTED:api-key]",
        }
    }

    /// Stable machine-readable name, mirroring the serde tag — watcher log
    /// lines and S03 counters share this kebab-case vocabulary.
    pub fn as_str(self) -> &'static str {
        match self {
            DetectionKind::Password => "password",
            DetectionKind::Card => "card",
            DetectionKind::ApiKey => "api-key",
        }
    }

    fn index(self) -> usize {
        match self {
            DetectionKind::Password => 0,
            DetectionKind::Card => 1,
            DetectionKind::ApiKey => 2,
        }
    }
}

/// One detector's aggregate result: kind and count only, never original text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Detection {
    pub kind: DetectionKind,
    pub count: usize,
}

/// Deterministic signal S02's fail-closed policy consumes. `Low` means the
/// engine saw something it could not fully vouch for — conditions are few,
/// operationally defined, and individually unit-tested:
/// 1. input exceeded [`SCAN_CAP_BYTES`];
/// 2. a candidate matched a detector's coarse pattern but failed validation
///    next to strong context words (a Luhn-failing 13–19 digit run on a line
///    containing a card context word).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RedactionConfidence {
    Confident,
    Low,
}

/// Result of a successful redaction pass.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RedactionOutcome {
    /// Input text with typed placeholders substituted for detected values.
    pub text: String,
    /// Kinds and counts only, in [`DetectionKind::ALL`] order; kinds with
    /// zero hits are omitted.
    pub detections: Vec<Detection>,
    pub confidence: RedactionConfidence,
}

/// Genuine engine failure. With `LazyLock`-compiled patterns the runtime
/// path is nearly unreachable, but the type must exist so the watcher mount
/// can fail closed (drop the observation) instead of ever panicking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedactionError {
    /// A detector pattern failed to compile. Carries the detector name only
    /// — never input text.
    PatternCompile { detector: &'static str },
}

impl RedactionError {
    /// Stable machine-readable error kind, matching every other kind-tagged
    /// error surface — the watcher logs fail-closed drops by this name.
    pub fn kind(&self) -> &'static str {
        match self {
            RedactionError::PatternCompile { .. } => "pattern-compile",
        }
    }
}

impl std::fmt::Display for RedactionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RedactionError::PatternCompile { detector } => {
                write!(f, "redaction pattern failed to compile: {detector}")
            }
        }
    }
}

impl std::error::Error for RedactionError {}

/// A lazily compiled pattern that surfaces compile failure as a typed error
/// instead of a panic (the engine must be panic-free per D028).
struct Pattern {
    detector: &'static str,
    compiled: LazyLock<Result<Regex, regex::Error>>,
}

impl Pattern {
    fn get(&self) -> Result<&Regex, RedactionError> {
        match &*self.compiled {
            Ok(re) => Ok(re),
            Err(_) => Err(RedactionError::PatternCompile {
                detector: self.detector,
            }),
        }
    }
}

macro_rules! pattern {
    ($name:ident, $detector:literal, $re:literal) => {
        static $name: Pattern = Pattern {
            detector: $detector,
            compiled: LazyLock::new(|| Regex::new($re)),
        };
    };
}

// Password: line-oriented context rule (watcher text is lines joined in
// Vision reading order). A password word followed by `:`/`=` redacts the
// value token after the separator. Longest alternatives first.
pattern!(
    PASSWORD_RE,
    "password",
    r"(?i)\b((?:passphrase|password|passwd|pwd|pass)\b\s*[:=]\s*)(\S+)"
);

// Known-prefix API keys: high-precision vendor prefixes, no context needed.
pattern!(
    KEY_PREFIX_RE,
    "api-key-prefix",
    r"(?x)
      \bsk-[A-Za-z0-9_-]{16,}
    | \bghp_[A-Za-z0-9]{20,}
    | \bgho_[A-Za-z0-9]{20,}
    | \bgithub_pat_[A-Za-z0-9_]{20,}
    | \bglpat-[A-Za-z0-9_-]{20,}
    | \bAKIA[0-9A-Z]{16}\b
    | \bxox[baprs]-[A-Za-z0-9-]{10,}
    | \bAIza[0-9A-Za-z_-]{30,}
    "
);

// PEM private-key blocks (multi-line, so applied to the full text).
pattern!(
    PEM_BLOCK_RE,
    "pem-block",
    r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----"
);

// Generic secret candidate: long base64/hex-charset run. Only redacted when
// entropy is high AND a context word sits on the same line — the context
// requirement keeps editor/terminal false positives tolerable.
pattern!(GENERIC_TOKEN_RE, "generic-token", r"[A-Za-z0-9+/=_-]{20,}");

pattern!(
    KEY_CONTEXT_RE,
    "key-context",
    r"(?i)\b(?:key|token|secret|api|bearer|credential|auth)\b"
);

// Card: standalone runs of 13–19 digits allowing space/dash separators (OCR
// renders `4111 1111 1111 1111`); word boundaries keep longer digit runs and
// ordinary prose numbers untouched. Luhn decides redaction.
pattern!(CARD_RE, "card", r"\b\d(?:[ -]?\d){12,18}\b");

pattern!(
    CARD_CONTEXT_RE,
    "card-context",
    r"(?i)\b(?:card|visa|mastercard|amex|credit|debit|cc)\b"
);

/// Redact sensitive values from `text`, substituting typed placeholders.
///
/// Pure, offline, deterministic, panic-free. The outcome carries the
/// redacted text, per-kind detection counts (never original text), and a
/// deterministic confidence signal for S02.
pub fn redact(text: &str) -> Result<RedactionOutcome, RedactionError> {
    let mut counts = [0usize; 3];
    let mut low_confidence = text.len() > SCAN_CAP_BYTES;

    // PEM blocks span lines, so handle them on the full text first.
    let pem_re = PEM_BLOCK_RE.get()?;
    let mut pem_hits = 0usize;
    let text = pem_re
        .replace_all(text, |_: &regex::Captures| {
            pem_hits += 1;
            DetectionKind::ApiKey.placeholder().to_string()
        })
        .into_owned();
    counts[DetectionKind::ApiKey.index()] += pem_hits;

    // Everything else is line-oriented: context words must sit on the same
    // line as the candidate, and the password rule is inherently per-line.
    let redacted_lines: Vec<String> = {
        let mut out = Vec::new();
        for line in text.split('\n') {
            out.push(redact_line(line, &mut counts, &mut low_confidence)?);
        }
        out
    };

    let detections = DetectionKind::ALL
        .into_iter()
        .filter(|kind| counts[kind.index()] > 0)
        .map(|kind| Detection {
            kind,
            count: counts[kind.index()],
        })
        .collect();

    Ok(RedactionOutcome {
        text: redacted_lines.join("\n"),
        detections,
        confidence: if low_confidence {
            RedactionConfidence::Low
        } else {
            RedactionConfidence::Confident
        },
    })
}

fn redact_line(
    line: &str,
    counts: &mut [usize; 3],
    low_confidence: &mut bool,
) -> Result<String, RedactionError> {
    // Context is judged on the original line: placeholders substituted below
    // contain words like "api-key" and must not manufacture context.
    let has_key_context = KEY_CONTEXT_RE.get()?.is_match(line);
    let has_card_context = CARD_CONTEXT_RE.get()?.is_match(line);

    // 1. Password rule (first, so a high-entropy password value is counted
    //    as a password, not a generic token).
    let mut hits = 0usize;
    let line = PASSWORD_RE
        .get()?
        .replace_all(line, |caps: &regex::Captures| {
            hits += 1;
            format!("{}{}", &caps[1], DetectionKind::Password.placeholder())
        })
        .into_owned();
    counts[DetectionKind::Password.index()] += hits;

    // 2. Known-prefix API keys (before the generic rule, so a prefixed key
    //    is counted once).
    let mut hits = 0usize;
    let line = KEY_PREFIX_RE
        .get()?
        .replace_all(&line, |_: &regex::Captures| {
            hits += 1;
            DetectionKind::ApiKey.placeholder().to_string()
        })
        .into_owned();
    counts[DetectionKind::ApiKey.index()] += hits;

    // 3. Generic high-entropy token, gated on same-line context.
    let mut hits = 0usize;
    let line = GENERIC_TOKEN_RE
        .get()?
        .replace_all(&line, |caps: &regex::Captures| {
            let token = &caps[0];
            if has_key_context && is_secret_like(token) {
                hits += 1;
                DetectionKind::ApiKey.placeholder().to_string()
            } else {
                token.to_string()
            }
        })
        .into_owned();
    counts[DetectionKind::ApiKey.index()] += hits;

    // 4. Card candidates: redact only Luhn-valid runs. A Luhn-failing
    //    candidate next to card context words lowers confidence (coarse
    //    pattern matched, validation failed — D028 rationale).
    let mut hits = 0usize;
    let mut luhn_reject_near_context = false;
    let line = CARD_RE
        .get()?
        .replace_all(&line, |caps: &regex::Captures| {
            let digits: String = caps[0].chars().filter(char::is_ascii_digit).collect();
            if luhn_valid(&digits) {
                hits += 1;
                DetectionKind::Card.placeholder().to_string()
            } else {
                if has_card_context {
                    luhn_reject_near_context = true;
                }
                caps[0].to_string()
            }
        })
        .into_owned();
    counts[DetectionKind::Card.index()] += hits;
    if luhn_reject_near_context {
        *low_confidence = true;
    }

    Ok(line)
}

/// Generic-token validation: long enough, mixes letters and digits (real
/// secrets nearly always do; rules out all-letter prose runs), and has high
/// Shannon entropy.
fn is_secret_like(token: &str) -> bool {
    token.len() >= GENERIC_TOKEN_MIN_LEN
        && token.chars().any(|c| c.is_ascii_digit())
        && token.chars().any(|c| c.is_ascii_alphabetic())
        && shannon_entropy_bits_per_char(token) >= GENERIC_TOKEN_MIN_ENTROPY
}

/// Shannon entropy in bits per character over the token's bytes.
fn shannon_entropy_bits_per_char(s: &str) -> f64 {
    let bytes = s.as_bytes();
    if bytes.is_empty() {
        return 0.0;
    }
    let mut freq = [0usize; 256];
    for &b in bytes {
        freq[b as usize] += 1;
    }
    let len = bytes.len() as f64;
    freq.iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = c as f64 / len;
            -p * p.log2()
        })
        .sum()
}

/// Standard Luhn checksum over a contiguous digit string.
fn luhn_valid(digits: &str) -> bool {
    if digits.is_empty() {
        return false;
    }
    let mut sum = 0u32;
    let mut double = false;
    for ch in digits.chars().rev() {
        let Some(mut d) = ch.to_digit(10) else {
            return false;
        };
        if double {
            d *= 2;
            if d > 9 {
                d -= 9;
            }
        }
        sum += d;
        double = !double;
    }
    sum.is_multiple_of(10)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn redact_ok(text: &str) -> RedactionOutcome {
        redact(text).expect("engine must not fail on valid input")
    }

    fn count_of(outcome: &RedactionOutcome, kind: DetectionKind) -> usize {
        outcome
            .detections
            .iter()
            .find(|d| d.kind == kind)
            .map(|d| d.count)
            .unwrap_or(0)
    }

    // --- Placeholder vocabulary: byte-exact contract (S02/S03/distillation) ---

    #[test]
    fn placeholder_literals_are_pinned_byte_exact() {
        assert_eq!(DetectionKind::Password.placeholder(), "[REDACTED:password]");
        assert_eq!(DetectionKind::Card.placeholder(), "[REDACTED:card]");
        assert_eq!(DetectionKind::ApiKey.placeholder(), "[REDACTED:api-key]");
    }

    #[test]
    fn password_line_redacts_to_exact_placeholder() {
        let out = redact_ok("password: hunter2");
        assert_eq!(out.text, "password: [REDACTED:password]");
        assert_eq!(
            out.detections,
            vec![Detection {
                kind: DetectionKind::Password,
                count: 1
            }]
        );
    }

    // --- Serde shape: camelCase fields, kebab-case kind tags (S03 contract) ---

    #[test]
    fn detection_serde_shape_is_camel_case_fields_kebab_case_kinds() {
        let d = Detection {
            kind: DetectionKind::ApiKey,
            count: 2,
        };
        assert_eq!(
            serde_json::to_value(&d).unwrap(),
            json!({"kind": "api-key", "count": 2})
        );
        let d = Detection {
            kind: DetectionKind::Password,
            count: 1,
        };
        assert_eq!(
            serde_json::to_value(&d).unwrap(),
            json!({"kind": "password", "count": 1})
        );
        let d = Detection {
            kind: DetectionKind::Card,
            count: 3,
        };
        assert_eq!(
            serde_json::to_value(&d).unwrap(),
            json!({"kind": "card", "count": 3})
        );
    }

    #[test]
    fn confidence_serde_is_kebab_case() {
        assert_eq!(
            serde_json::to_value(RedactionConfidence::Confident).unwrap(),
            json!("confident")
        );
        assert_eq!(
            serde_json::to_value(RedactionConfidence::Low).unwrap(),
            json!("low")
        );
    }

    // --- Card detector: Luhn-gated, spaced and contiguous ---

    #[test]
    fn luhn_valid_card_spaced_is_redacted() {
        let out = redact_ok("checkout with 4111 1111 1111 1111 today");
        assert_eq!(out.text, "checkout with [REDACTED:card] today");
        assert_eq!(count_of(&out, DetectionKind::Card), 1);
    }

    #[test]
    fn luhn_valid_card_contiguous_is_redacted() {
        let out = redact_ok("pan=4111111111111111");
        assert_eq!(out.text, "pan=[REDACTED:card]");
        assert_eq!(count_of(&out, DetectionKind::Card), 1);
    }

    #[test]
    fn luhn_valid_card_dash_separated_is_redacted() {
        let out = redact_ok("4111-1111-1111-1111");
        assert_eq!(out.text, "[REDACTED:card]");
    }

    #[test]
    fn luhn_invalid_digit_run_passes_through() {
        let input = "order ref 4111 1111 1111 1112 shipped";
        let out = redact_ok(input);
        assert_eq!(out.text, input);
        assert!(out.detections.is_empty());
    }

    #[test]
    fn short_and_long_digit_runs_pass_through() {
        // Under 13 digits: ordinary prose numbers.
        let out = redact_ok("call 555 0123 before 2026");
        assert_eq!(out.text, "call 555 0123 before 2026");
        // Over 19 digits: not a standalone card candidate.
        let input = "serial 41111111111111111111111111";
        let out = redact_ok(input);
        assert_eq!(out.text, input);
    }

    // --- API key detector: known prefixes ---

    #[test]
    fn openai_sk_key_is_redacted() {
        let out = redact_ok("export OPENAI_API_KEY=sk-abc123def456ghi789jkl012");
        assert_eq!(out.text, "export OPENAI_API_KEY=[REDACTED:api-key]");
        assert_eq!(count_of(&out, DetectionKind::ApiKey), 1);
    }

    #[test]
    fn github_ghp_key_is_redacted() {
        let out = redact_ok("token ghp_AbCdEf123456789012345678901234567890 saved");
        assert_eq!(out.text, "token [REDACTED:api-key] saved");
        assert_eq!(count_of(&out, DetectionKind::ApiKey), 1);
    }

    #[test]
    fn aws_akia_key_is_redacted() {
        let out = redact_ok("aws_access_key_id = AKIAIOSFODNN7EXAMPLE");
        assert_eq!(out.text, "aws_access_key_id = [REDACTED:api-key]");
    }

    #[test]
    fn pem_private_key_block_is_redacted() {
        let input = "notes\n-----BEGIN RSA PRIVATE KEY-----\nMIIEow==\nabc\n-----END RSA PRIVATE KEY-----\ndone";
        let out = redact_ok(input);
        assert_eq!(out.text, "notes\n[REDACTED:api-key]\ndone");
        assert_eq!(count_of(&out, DetectionKind::ApiKey), 1);
    }

    // --- API key detector: generic entropy + same-line context ---

    #[test]
    fn high_entropy_token_with_context_is_redacted() {
        let out = redact_ok("api token: A8f3kQ9zL2mX7pR4wN6vB1cJ");
        assert_eq!(out.text, "api token: [REDACTED:api-key]");
        assert_eq!(count_of(&out, DetectionKind::ApiKey), 1);
    }

    #[test]
    fn high_entropy_token_without_context_passes_through() {
        let input = "A8f3kQ9zL2mX7pR4wN6vB1cJ";
        let out = redact_ok(input);
        assert_eq!(out.text, input);
        assert!(out.detections.is_empty());
    }

    #[test]
    fn low_entropy_token_with_context_passes_through() {
        let input = "token: aaaaaaaaaaaaaaaa1111";
        let out = redact_ok(input);
        assert_eq!(out.text, input);
        assert!(out.detections.is_empty());
    }

    #[test]
    fn all_letter_run_with_context_passes_through() {
        // No digit → not secret-like, even next to a context word.
        let input = "secret ingredient: extraordinarily/delicious";
        let out = redact_ok(input);
        assert_eq!(out.text, input);
        assert!(out.detections.is_empty());
    }

    // --- Negative surface: innocent dev-screen fixture ---

    #[test]
    fn innocent_dev_screen_fixture_is_untouched() {
        let fixture = "fn main() { println!(\"hello\"); }\n\
                       let x = compute_value(42);\n\
                       ts=1721390000000 status=ok\n\
                       Meeting notes: discuss quarterly budget\n\
                       https://example.com/docs/getting-started";
        let out = redact_ok(fixture);
        assert_eq!(out.text, fixture);
        assert!(out.detections.is_empty());
        assert_eq!(out.confidence, RedactionConfidence::Confident);
    }

    // --- Password rule variants ---

    #[test]
    fn password_word_variants_are_redacted() {
        for line in [
            "passwd=s3cr3t!",
            "Passphrase: correct-horse-battery",
            "pwd = qwerty99",
            "PASSWORD: Tr0ub4dor&3",
        ] {
            let out = redact_ok(line);
            assert!(
                out.text.contains("[REDACTED:password]"),
                "expected password redaction in {line:?}, got {:?}",
                out.text
            );
            assert_eq!(count_of(&out, DetectionKind::Password), 1, "line {line:?}");
        }
    }

    #[test]
    fn detections_never_carry_original_text() {
        let out = redact_ok("password: hunter2");
        let serialized = serde_json::to_string(&out.detections).unwrap();
        assert!(!serialized.contains("hunter2"));
        assert_eq!(serialized, r#"[{"kind":"password","count":1}]"#);
    }

    // --- Confidence signal: few, deterministic, individually tested ---

    #[test]
    fn oversized_input_lowers_confidence() {
        let big = "a".repeat(SCAN_CAP_BYTES + 1);
        let out = redact_ok(&big);
        assert_eq!(out.confidence, RedactionConfidence::Low);
        // At the cap exactly: still confident.
        let at_cap = "a".repeat(SCAN_CAP_BYTES);
        assert_eq!(
            redact_ok(&at_cap).confidence,
            RedactionConfidence::Confident
        );
    }

    #[test]
    fn luhn_reject_near_card_context_lowers_confidence() {
        let out = redact_ok("credit card: 4111 1111 1111 1112");
        assert_eq!(out.confidence, RedactionConfidence::Low);
        assert!(out.detections.is_empty());
        // Same Luhn-failing run without card context: confident.
        let out = redact_ok("ref 4111 1111 1111 1112");
        assert_eq!(out.confidence, RedactionConfidence::Confident);
    }

    // --- Determinism and aggregation ---

    #[test]
    fn redaction_is_deterministic_across_calls() {
        let input = "password: hunter2\ncard 4111 1111 1111 1111\nkey sk-abc123def456ghi789jkl012";
        let a = redact_ok(input);
        let b = redact_ok(input);
        assert_eq!(a, b);
    }

    #[test]
    fn multiple_hits_aggregate_counts_in_stable_order() {
        let input = "password: one2three\n\
                     pwd: four5six\n\
                     4111 1111 1111 1111 and 5555 5555 5555 4444\n\
                     sk-abc123def456ghi789jkl012";
        let out = redact_ok(input);
        assert_eq!(
            out.detections,
            vec![
                Detection {
                    kind: DetectionKind::Password,
                    count: 2
                },
                Detection {
                    kind: DetectionKind::Card,
                    count: 2
                },
                Detection {
                    kind: DetectionKind::ApiKey,
                    count: 1
                },
            ]
        );
        assert!(!out.text.contains("4444"));
        assert!(!out.text.contains("three"));
    }

    #[test]
    fn empty_and_whitespace_inputs_are_no_ops() {
        for input in ["", "\n", "   \n\t\n"] {
            let out = redact_ok(input);
            assert_eq!(out.text, input);
            assert!(out.detections.is_empty());
            assert_eq!(out.confidence, RedactionConfidence::Confident);
        }
    }

    #[test]
    fn placeholders_do_not_manufacture_context_for_later_lines() {
        // "[REDACTED:api-key]" contains "api"/"key" — context must be judged
        // on the original line, so an adjacent innocent high-entropy token
        // on a context-free line stays untouched.
        let input = "ghp_AbCdEf123456789012345678901234567890 X9y8Z7w6V5u4T3s2R1q0Pa";
        let out = redact_ok(input);
        assert_eq!(out.text, "[REDACTED:api-key] X9y8Z7w6V5u4T3s2R1q0Pa");
        assert_eq!(count_of(&out, DetectionKind::ApiKey), 1);
    }
}
