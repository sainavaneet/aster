//! Secret redaction for text that leaves a worker's screen or report: the
//! lead-facing blocked-prompt excerpt and every decision-layer input (approval
//! command and prompt, report sanity, interruption screen tail). Redaction
//! errs on the side of removing too much.

const REDACTED: &str = "<redacted>";
/// Report tokens are 32 hex digits; any run this long in prompt text is redacted.
const TOKEN_LIKE_HEX_DIGITS: usize = 32;

/// Hex digits split only by line breaks (and indentation): a token the terminal
/// wrapped across lines. Each piece is replaced, keeping the line structure.
#[derive(Default)]
struct HexRun {
    /// Hex pieces, each with the whitespace that followed it.
    pieces: Vec<(String, String)>,
    digits: usize,
}

impl HexRun {
    fn push_digit(&mut self, ch: char) {
        match self.pieces.last_mut() {
            Some((piece, gap)) if gap.is_empty() => piece.push(ch),
            _ => self.pieces.push((ch.to_string(), String::new())),
        }
        self.digits += 1;
    }

    /// Whitespace continues the run only while it may hold a line break.
    fn push_space(&mut self, ch: char) -> bool {
        let Some((_, gap)) = self.pieces.last_mut() else {
            return false;
        };
        gap.push(ch);
        true
    }

    /// Ends at a non-hex character, or at whitespace without a line break.
    fn flush(&mut self, out: &mut String) {
        let redact = self.digits >= TOKEN_LIKE_HEX_DIGITS;
        for (piece, gap) in self.pieces.drain(..) {
            out.push_str(if redact { REDACTED } else { &piece });
            out.push_str(&gap);
        }
        self.digits = 0;
    }
}

/// Replaces token-like hex runs, so a prompt quoting a report command cannot
/// relay the worker's token. A run continues across a line break, so a token
/// the terminal wrapped is still recognised; spaces within a line end it.
fn redact_hex_runs(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut run = HexRun::default();
    for ch in text.chars() {
        if ch.is_ascii_hexdigit() {
            run.push_digit(ch);
            continue;
        }
        let gap_has_break = run.pieces.last().is_some_and(|(_, gap)| gap.contains('\n'));
        if ch.is_whitespace() && (ch == '\n' || gap_has_break) && run.push_space(ch) {
            continue;
        }
        run.flush(&mut out);
        out.push(ch);
    }
    run.flush(&mut out);
    out
}

/// Characters of a credential token; quotes and whitespace end it.
const TOKEN_CHARS: &str = r#"[^\s'"`]"#;

/// How a pattern's match is replaced.
#[derive(Clone, Copy)]
enum Replace {
    /// `$1` (the label) followed by the replacement text.
    Labelled(&'static str),
    /// Every line of the match becomes `<redacted>`, keeping its indentation,
    /// so a token the terminal wrapped onto the next line goes too.
    Lines,
}

/// Common credential shapes, in the order they are applied: PEM blocks,
/// `Authorization:` headers, bearer tokens, `password=`-style values, provider
/// key prefixes, and JWTs.
fn secret_patterns() -> &'static [(regex::Regex, Replace)] {
    static PATTERNS: std::sync::OnceLock<Vec<(regex::Regex, Replace)>> = std::sync::OnceLock::new();
    PATTERNS.get_or_init(|| {
        // A token reaching the end of its line continues on the next one.
        let wrapped = format!(r"(?:\r?\n[ \t]*{TOKEN_CHARS}+)*");
        let prefixes =
            r"sk-|sk_live_|sk_test_|rk_live_|ghp_|gho_|ghs_|ghu_|github_pat_|glpat-|xox[abprs]-|AKIA";
        [
            (r"-----BEGIN[^\n]*".to_owned(), Replace::Labelled("")),
            (
                r"(?i)\b(authorization)\s*:[^\n]*".to_owned(),
                Replace::Labelled(": <redacted>"),
            ),
            (
                r"(?i)\b(bearer)\s+[^\s'`]+".to_owned(),
                Replace::Labelled(" <redacted>"),
            ),
            (
                r#"(?i)([a-z0-9_]*(?:password|passwd|token|secret|api[_-]?key))=(?:"[^"]*"|'[^']*'|[^\s'"`&;]+)"#
                    .to_owned(),
                Replace::Labelled("=<redacted>"),
            ),
            (
                format!(r"\b(?:{prefixes}){TOKEN_CHARS}*{wrapped}"),
                Replace::Lines,
            ),
            (format!(r"\bASIA[A-Z0-9]{{12,}}{wrapped}"), Replace::Lines),
            (
                format!(r"\beyJ[A-Za-z0-9_-]+\.eyJ[A-Za-z0-9_-]+\.{TOKEN_CHARS}*{wrapped}"),
                Replace::Lines,
            ),
        ]
        .into_iter()
        .map(|(pattern, replace)| {
            (
                regex::Regex::new(&pattern).expect("valid secret pattern"),
                replace,
            )
        })
        .collect()
    })
}

/// `<redacted>` for each line of `matched`, keeping each line's indentation.
fn redact_lines(matched: &str) -> String {
    matched
        .split('\n')
        .map(|line| {
            let indent = line.len() - line.trim_start().len();
            let carriage = if line.ends_with('\r') { "\r" } else { "" };
            format!("{}{REDACTED}{carriage}", &line[..indent])
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn redact_secrets(text: &str) -> String {
    let mut text = redact_hex_runs(text);
    for (pattern, replace) in secret_patterns() {
        text = match replace {
            Replace::Labelled("") => pattern.replace_all(&text, REDACTED).into_owned(),
            Replace::Labelled(suffix) => pattern
                .replace_all(&text, |captures: &regex::Captures<'_>| {
                    format!("{}{suffix}", &captures[1])
                })
                .into_owned(),
            Replace::Lines => pattern
                .replace_all(&text, |captures: &regex::Captures<'_>| {
                    redact_lines(&captures[0])
                })
                .into_owned(),
        };
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_supported_secret_format_is_redacted() {
        let cases = [
            ("key sk-abcDEF0123456789 end", "key <redacted> end"),
            ("stripe sk_live_51HabcDEF now", "stripe <redacted> now"),
            (
                "stripe sk_test_4eC39HqLyjWDarjtT1 now",
                "stripe <redacted> now",
            ),
            ("gh ghp_16C7e42F292c6912E7 now", "gh <redacted> now"),
            ("gh gho_16C7e42F292c6912E7 now", "gh <redacted> now"),
            (
                "pat github_pat_11ABCDEFG0_abcdefXYZ now",
                "pat <redacted> now",
            ),
            (
                "gitlab glpat-xYz123AbC456dEf789 now",
                "gitlab <redacted> now",
            ),
            ("slack xoxb-1234-abcd now", "slack <redacted> now"),
            ("slack xoxa-2-abc now", "slack <redacted> now"),
            ("slack xoxp-99-abc now", "slack <redacted> now"),
            ("aws AKIAIOSFODNN7EXAMPLE now", "aws <redacted> now"),
            ("sts ASIAIOSFODNN7EXAMPLE now", "sts <redacted> now"),
            (
                "jwt eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.c2lnbmF0dXJl now",
                "jwt <redacted> now",
            ),
            (
                "login password=hunter2 now",
                "login password=<redacted> now",
            ),
            ("login passwd='hunter 2' now", "login passwd=<redacted> now"),
            ("url ?token=abc123&x=1", "url ?token=<redacted>&x=1"),
            (
                "env DB_SECRET=\"s3 cr3t\" now",
                "env DB_SECRET=<redacted> now",
            ),
            (
                "env OPENAI_API_KEY=abc now",
                "env OPENAI_API_KEY=<redacted> now",
            ),
            ("Use Bearer abc.def.ghi now", "Use Bearer <redacted> now"),
            (
                "curl -H 'Authorization: Basic dXNlcjpwYXNz' x\nnext",
                "curl -H 'Authorization: <redacted>\nnext",
            ),
            (
                "key -----BEGIN RSA PRIVATE KEY----- MIIE\nnext",
                "key <redacted>\nnext",
            ),
            ("hex 0123456789abcdef0123456789abcdef", "hex <redacted>"),
        ];
        for (text, expected) in cases {
            assert_eq!(redact_secrets(text), expected, "{text}");
        }
    }

    #[test]
    fn prefixed_tokens_wrapped_across_lines_are_redacted_on_both_lines() {
        for prefix in [
            "sk-",
            "sk_live_",
            "sk_test_",
            "ghp_",
            "gho_",
            "github_pat_",
            "glpat-",
            "xoxb-",
            "AKIA",
        ] {
            let text = format!("Allow {prefix}HEADPART\n    TAILPART for this?\n");
            let redacted = redact_secrets(&text);
            assert!(!redacted.contains("HEADPART"), "{prefix}: {redacted}");
            assert!(!redacted.contains("TAILPART"), "{prefix}: {redacted}");
            assert_eq!(
                redacted, "Allow <redacted>\n    <redacted> for this?\n",
                "{prefix}"
            );
        }
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.sig\nnature rest";
        assert_eq!(redact_secrets(jwt), "<redacted>\n<redacted> rest");
    }

    #[test]
    fn ordinary_words_are_kept() {
        for text in [
            "Run the task-runner for ask-me?",
            "the token is valid; no secrets here",
            "password reset docs",
            "skip sk without dash",
            "ASIAN markets",
        ] {
            assert_eq!(redact_secrets(text), text);
        }
    }
}
