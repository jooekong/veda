use veda_core::store::LlmService;
use veda_types::Result;

// Prompt template variables supported (replaced before sending to LLM):
//   {content}    — file content (truncated)
//   {children}   — newline-joined "[N]. summary" for directory aggregation
//   {overview}   — directory L1 text used to derive directory L0
//   {language}   — auto-detected output language ("en" | "zh-CN") so the
//                  summary is in the same language as the source.
//
// Single-file prompts (L0/L1) and directory prompts (L1 from children, L0
// from L1) live in this file; the worker selects between them based on
// whether the dentry is a file or a directory.

const L0_PROMPT: &str = r#"Output language: {language}.

Summarize the following content in ONE concise sentence (max ~100 tokens).
Focus on what this content IS and its primary purpose. Do not include
meta-commentary or hedging phrases.

Content:
---
{content}
---

One-sentence summary:"#;

const L1_PROMPT: &str = r#"Output language: {language}.

Generate a structured overview document for the following file. Use the
exact Markdown layout below and stay under ~2000 tokens.

# <File-name or one-line title>

<Brief Description: a 50-150 word paragraph immediately after the title,
NO heading. Explain what this file is, what it covers, who it's for, and
include the core keywords for search.>

## Key Sections
- **<heading>** – <one-line description>
- ...

## Important Concepts / APIs
- <name> – <one-line definition or signature>
- ...

## Navigation Hints
- For X → see <section/subsection name>
- ...

Content:
---
{content}
---

Structured overview:"#;

const L1_DIR_PROMPT: &str = r#"Output language: {language}.

Generate an overview document for a directory based on the listed children.
Treat children as parts of the same project unless their summaries clearly
indicate independent projects. Use this exact Markdown layout, total length
400-800 words.

# <Directory>

<Brief Description: 50-150 word plain paragraph right after the title (no
sub-heading). What this directory is, what it covers, and who it's for.
Include core keywords for search.>

## Quick Navigation
Use a decision-tree style. Phrase as "What do you want to learn / do?"
Each line ends with → and references children by their numeric label
([1], [2], ...). Concise keyword descriptions only.
- What do you want to ...? → see [N] <one-line>
- ...

## Detailed Description
One H3 subsection per child, reusing the child's summary as the body.

### [1]
<reuse the provided summary>

### [2]
...

Children (numbered):
---
{children}
---

Overview:"#;

const DIR_L0_PROMPT: &str = r#"Output language: {language}.

Summarize the following directory overview in ONE concise sentence
(max ~100 tokens). Focus on what the directory contains as a whole.

Overview:
---
{overview}
---

One-sentence summary:"#;

/// Detect a coarse output language from a sample of the content. Returns a
/// label suitable for the {language} prompt slot. Heuristic only — looks at
/// the first 500 chars and counts unique-script signals separately:
/// Hiragana/Katakana → `ja`, Hangul → `ko`, Han (CJK Unified / Extension A)
/// → `zh-CN`. The first to cross 25% wins, in that order, since kana and
/// hangul are unique to their language while Han chars are shared between
/// Chinese and Japanese kanji-only segments. Anything else → `en`.
pub fn detect_output_language(sample: &str) -> &'static str {
    let chars: Vec<char> = sample.chars().take(500).collect();
    let n = chars.len();
    if n == 0 {
        return "en";
    }
    let mut kana = 0usize;
    let mut hangul = 0usize;
    let mut han = 0usize;
    for c in &chars {
        let code = *c as u32;
        if (0x3040..=0x309F).contains(&code) || (0x30A0..=0x30FF).contains(&code) {
            kana += 1;
        } else if (0xAC00..=0xD7AF).contains(&code) {
            hangul += 1;
        } else if (0x4E00..=0x9FFF).contains(&code) || (0x3400..=0x4DBF).contains(&code) {
            han += 1;
        }
    }
    // Threshold = strictly over 25%. Order matters: kana/hangul are unique
    // markers; Han comes last so a Japanese sample with kanji + hiragana
    // resolves to `ja`, not `zh-CN`.
    if kana * 4 > n {
        "ja"
    } else if hangul * 4 > n {
        "ko"
    } else if han * 4 > n {
        "zh-CN"
    } else {
        "en"
    }
}

pub async fn generate_l0(llm: &dyn LlmService, content: &str) -> Result<String> {
    let truncated = truncate_content(content, 12_000);
    let lang = detect_output_language(&truncated);
    let prompt = L0_PROMPT
        .replace("{language}", lang)
        .replace("{content}", &truncated);
    llm.summarize(&prompt, 150).await
}

pub async fn generate_l1(llm: &dyn LlmService, content: &str, max_tokens: usize) -> Result<String> {
    let truncated = truncate_content(content, 12_000);
    let lang = detect_output_language(&truncated);
    let prompt = L1_PROMPT
        .replace("{language}", lang)
        .replace("{content}", &truncated);
    llm.summarize(&prompt, max_tokens).await
}

pub async fn aggregate_dir_summary(
    llm: &dyn LlmService,
    child_l0s: &[String],
    max_overview_tokens: usize,
) -> Result<(String, String)> {
    let children_text = child_l0s
        .iter()
        .enumerate()
        .map(|(i, s)| format!("[{}]. {}", i + 1, s))
        .collect::<Vec<_>>()
        .join("\n");
    // Cap children_text so a directory with thousands of children doesn't
    // blow the LLM context window or run up cost. 8000 chars ~ 2k tokens.
    // Truncating happens after numbering so the keep portion still has
    // numeric refs that match the prompt's `[N]` notation.
    let children_text = truncate_content(&children_text, 8_000);

    // Use the children's combined text to detect the directory's dominant
    // language; mixed-language repos fall back to en.
    let lang = detect_output_language(&children_text);

    let l1_prompt = L1_DIR_PROMPT
        .replace("{language}", lang)
        .replace("{children}", &children_text);
    let l1 = llm.summarize(&l1_prompt, max_overview_tokens).await?;

    let l0_prompt = DIR_L0_PROMPT
        .replace("{language}", lang)
        .replace("{overview}", &l1);
    let l0 = llm.summarize(&l0_prompt, 150).await?;

    Ok((l0, l1))
}

fn truncate_content(content: &str, max_chars: usize) -> String {
    // Count chars, not bytes — `len()` is byte length so CJK text would
    // truncate at roughly 1/3 of the intended limit. `nth(max_chars)`
    // short-circuits as soon as the iterator passes the cap, so we don't
    // walk the whole string for short inputs.
    if content.chars().nth(max_chars).is_none() {
        return content.to_string();
    }
    let half = max_chars / 2;
    let start: String = content.chars().take(half).collect();
    let end: String = content
        .chars()
        .rev()
        .take(half)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!("{start}\n\n[... content truncated ...]\n\n{end}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    struct MockLlm;

    #[async_trait]
    impl LlmService for MockLlm {
        async fn summarize(&self, content: &str, _max_tokens: usize) -> Result<String> {
            Ok(format!("SUMMARY: {}", &content[..content.len().min(50)]))
        }
    }

    #[test]
    fn truncate_short_content() {
        let s = "hello world";
        assert_eq!(truncate_content(s, 100), s);
    }

    #[test]
    fn truncate_long_content() {
        let s = "a".repeat(200);
        let result = truncate_content(&s, 100);
        assert!(result.contains("[... content truncated ...]"));
        assert!(result.len() < 200);
    }

    #[test]
    fn truncate_counts_chars_not_bytes() {
        // Each CJK char is 3 bytes in UTF-8. 200 chars = 600 bytes.
        // With cap=300, byte-based check would truncate; char-based must not.
        let s: String = "中".repeat(200);
        let result = truncate_content(&s, 300);
        assert_eq!(result, s, "200 CJK chars should not be truncated under cap=300");
    }

    #[test]
    fn truncate_cjk_actually_truncates_when_over() {
        let s: String = "中".repeat(200);
        let result = truncate_content(&s, 100);
        assert!(result.contains("[... content truncated ...]"));
    }

    #[tokio::test]
    async fn generate_l0_returns_summary() {
        let llm = MockLlm;
        let result = generate_l0(&llm, "This is a test document about Rust programming.").await;
        assert!(result.is_ok());
        let text = result.unwrap();
        assert!(text.starts_with("SUMMARY:"));
    }

    #[tokio::test]
    async fn generate_l1_returns_summary() {
        let llm = MockLlm;
        let result = generate_l1(
            &llm,
            "# Chapter 1\nSome content\n# Chapter 2\nMore content",
            2048,
        )
        .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn aggregate_dir_produces_l0_and_l1() {
        let llm = MockLlm;
        let children = vec![
            "API authentication guide".to_string(),
            "Database schema reference".to_string(),
        ];
        let result = aggregate_dir_summary(&llm, &children, 2048).await;
        assert!(result.is_ok());
        let (l0, l1) = result.unwrap();
        assert!(!l0.is_empty());
        assert!(!l1.is_empty());
    }

    #[test]
    fn detect_lang_empty_defaults_to_en() {
        assert_eq!(detect_output_language(""), "en");
    }

    #[test]
    fn detect_lang_pure_ascii() {
        assert_eq!(
            detect_output_language("The quick brown fox jumps over the lazy dog."),
            "en"
        );
    }

    #[test]
    fn detect_lang_pure_cjk() {
        assert_eq!(detect_output_language("敏捷的棕色狐狸跳过懒狗"), "zh-CN");
    }

    #[test]
    fn detect_lang_threshold_edge() {
        // Threshold is `cjk * 4 > len` (strict), i.e. just over 25%.
        // Exactly 1 CJK in 4 chars: 4 > 4 is false → "en".
        assert_eq!(detect_output_language("中abc"), "en");
        // 2 CJK in 7 chars: 8 > 7 → "zh-CN".
        assert_eq!(detect_output_language("中文abcde"), "zh-CN");
    }

    #[test]
    fn detect_lang_mixed_majority_cjk() {
        // ~50% CJK; well over the 25% threshold.
        assert_eq!(
            detect_output_language("Rust 是一门系统编程语言, focus on safety"),
            "zh-CN"
        );
    }

    #[test]
    fn detect_lang_japanese_hiragana() {
        assert_eq!(detect_output_language("こんにちは世界"), "ja");
    }

    #[test]
    fn detect_lang_japanese_katakana() {
        assert_eq!(detect_output_language("カタカナテスト"), "ja");
    }

    #[test]
    fn detect_lang_korean_hangul() {
        assert_eq!(detect_output_language("안녕하세요세계"), "ko");
    }

    #[test]
    fn detect_lang_japanese_with_kanji_resolves_to_ja_not_zh() {
        // Mixed kanji + hiragana — Japanese, not Chinese. Order of script
        // checks matters: kana wins over han.
        assert_eq!(detect_output_language("これは日本語のテスト"), "ja");
    }
}
