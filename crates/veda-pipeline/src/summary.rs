use veda_core::store::LlmService;
use veda_types::Result;

const L0_PROMPT: &str = r#"Summarize the following content in ONE concise sentence (max ~100 tokens).
Focus on what this content IS and its primary purpose. Do not include meta-commentary.

Content:
---
{content}
---

One-sentence summary:"#;

const L1_PROMPT: &str = r#"Create a structured overview (max ~2000 tokens) for the following content.
Include:
- A brief description of what this content covers
- Key sections or topics with one-line descriptions
- Important facts, APIs, or concepts mentioned
- Navigation hints (e.g. which sub-sections contain what)

Content:
---
{content}
---

Structured overview:"#;

const L1_DIR_PROMPT: &str = r#"Create a structured overview (max ~2000 tokens) for a directory that contains the following items.
Each item below is a one-sentence summary (L0 abstract) of a child file or subdirectory.

Child summaries:
---
{children}
---

Create a cohesive overview that:
- Describes what this directory contains as a whole
- Groups related items if applicable
- Highlights the most important items

Structured overview:"#;

const DIR_L0_PROMPT: &str = r#"Summarize the following directory overview in ONE concise sentence (max ~100 tokens).
Focus on what this directory contains as a whole.

Overview:
---
{overview}
---

One-sentence summary:"#;

pub async fn generate_l0(llm: &dyn LlmService, content: &str) -> Result<String> {
    let truncated = truncate_content(content, 12_000);
    let prompt = L0_PROMPT.replace("{content}", &truncated);
    llm.summarize(&prompt, 150).await
}

pub async fn generate_l1(llm: &dyn LlmService, content: &str, max_tokens: usize) -> Result<String> {
    let truncated = truncate_content(content, 12_000);
    let prompt = L1_PROMPT.replace("{content}", &truncated);
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
        .map(|(i, s)| format!("{}. {}", i + 1, s))
        .collect::<Vec<_>>()
        .join("\n");

    let l1_prompt = L1_DIR_PROMPT.replace("{children}", &children_text);
    let l1 = llm.summarize(&l1_prompt, max_overview_tokens).await?;

    let l0_prompt = DIR_L0_PROMPT.replace("{overview}", &l1);
    let l0 = llm.summarize(&l0_prompt, 150).await?;

    Ok((l0, l1))
}

fn truncate_content(content: &str, max_chars: usize) -> String {
    if content.len() <= max_chars {
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
}
