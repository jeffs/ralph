use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NitStatus {
    Open,
    Promoted,
    Dismissed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Nit {
    pub id: String,
    pub source_task: String,
    pub source_role: String,
    pub attempt: u32,
    pub content: String,
    #[serde(default)]
    pub summary: String,
    pub status: NitStatus,
    pub promoted_to: Option<String>,
    pub created_at: u64,
}

/// Generate a short (≤60 char) summary from nit content.
/// Takes the first line and truncates at a word boundary.
pub fn summarize(content: &str) -> String {
    let first_line = content.lines().next().unwrap_or(content).trim();
    truncate_with_ellipsis(first_line, 60)
}

/// Truncate a string to `max_len` chars, breaking at a word boundary when
/// possible and appending "…" if truncated.
pub fn truncate_with_ellipsis(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        return s.to_string();
    }
    let mut end = max_len;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    let search_range = &s[..end];
    if let Some(pos) = search_range.rfind(' ')
        && pos > max_len / 2
    {
        return format!("{}…", s[..pos].trim_end());
    }
    format!("{}…", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_nit(id: &str, status: NitStatus) -> Nit {
        Nit {
            id: id.into(),
            source_task: "T1".into(),
            source_role: "reviewer".into(),
            attempt: 1,
            content: "fix this".into(),
            summary: String::new(),
            status,
            promoted_to: None,
            created_at: 1000,
        }
    }

    #[test]
    fn roundtrip_jsonl() {
        let nit = Nit {
            id: "NIT-1".into(),
            source_task: "BUILD-4".into(),
            source_role: "reviewer".into(),
            attempt: 1,
            content: "manifest.json not updated".into(),
            summary: "Update manifest.json".into(),
            status: NitStatus::Open,
            promoted_to: None,
            created_at: 1708300000,
        };
        let json = serde_json::to_string(&nit).unwrap();
        let parsed: Nit = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, "NIT-1");
        assert_eq!(parsed.summary, "Update manifest.json");
        assert_eq!(parsed.status, NitStatus::Open);
        assert!(parsed.promoted_to.is_none());
    }

    #[test]
    fn backwards_compat_missing_summary() {
        // Pre-existing nits without the summary field should deserialize with default ""
        let json = r#"{"id":"NIT-1","source_task":"T1","source_role":"reviewer","attempt":1,"content":"fix","status":"open","promoted_to":null,"created_at":1000}"#;
        let nit: Nit = serde_json::from_str(json).unwrap();
        assert_eq!(nit.summary, "");
    }

    #[test]
    fn summarize_short_content() {
        assert_eq!(summarize("Fix the bug"), "Fix the bug");
    }

    #[test]
    fn summarize_multiline_uses_first_line() {
        let content = "Fix the bug\nMore details about it here\nAnd more";
        assert_eq!(summarize(content), "Fix the bug");
    }

    #[test]
    fn summarize_truncates_long_line() {
        let long =
            "Fix the meta tag self-closing style inconsistency across all HTML template files";
        let summary = summarize(long);
        assert!(summary.len() <= 64); // 60 + room for ellipsis char
        assert!(summary.ends_with('…'));
    }

    #[test]
    fn truncate_with_ellipsis_short() {
        assert_eq!(truncate_with_ellipsis("short", 60), "short");
    }

    #[test]
    fn truncate_with_ellipsis_at_word_boundary() {
        let s = "Fix the meta tag self-closing style inconsistency across all HTML files";
        let result = truncate_with_ellipsis(s, 60);
        assert!(result.len() <= 64);
        assert!(result.ends_with('…'));
        assert!(!result.ends_with(" …")); // no trailing space before ellipsis
    }

    #[test]
    fn truncate_with_ellipsis_multibyte_boundary() {
        // em-dash is 3 bytes; place it so byte 60 lands mid-character
        let s = "(1) New `<meta>` tag uses `>` while existing tags use `/>` — minor style inconsistency.";
        let result = truncate_with_ellipsis(s, 60);
        assert!(result.len() <= 64);
        assert!(result.ends_with('…'));
    }

    #[test]
    fn status_update_roundtrip() {
        let mut nit = make_nit("NIT-1", NitStatus::Open);
        nit.status = NitStatus::Promoted;
        nit.promoted_to = Some("NIT1".into());
        let json = serde_json::to_string(&nit).unwrap();
        let parsed: Nit = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.status, NitStatus::Promoted);
        assert_eq!(parsed.promoted_to.as_deref(), Some("NIT1"));
    }
}
