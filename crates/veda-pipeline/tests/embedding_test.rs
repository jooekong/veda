use std::path::PathBuf;

use serde::Deserialize;
use veda_core::store::EmbeddingService;
use veda_pipeline::embedding::EmbeddingProvider;
use veda_types::Result;

#[derive(Debug, Deserialize)]
struct TestConfig {
    embedding: EmbeddingSection,
}

#[derive(Debug, Deserialize)]
struct EmbeddingSection {
    api_url: String,
    api_key: String,
    model: String,
    dimension: Option<u32>,
}

fn load_test_config() -> EmbeddingSection {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/test.toml");
    let raw =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let cfg: TestConfig = toml::from_str(&raw).expect("parse test.toml");
    cfg.embedding
}

fn make_provider() -> EmbeddingProvider {
    let cfg = load_test_config();
    EmbeddingProvider::new(cfg.api_url, cfg.api_key, cfg.model, cfg.dimension).expect("provider")
}

#[tokio::test]
#[ignore]
async fn embedding_single_text() -> Result<()> {
    let provider = make_provider();
    let vecs = provider
        .embed(&["hello from veda-pipeline".to_string()])
        .await?;
    assert_eq!(vecs.len(), 1);
    let dim = cfg_dimension_or_vector_len(&provider, &vecs[0]);
    assert_eq!(vecs[0].len(), dim);
    Ok(())
}

#[tokio::test]
#[ignore]
async fn embedding_batch() -> Result<()> {
    let provider = make_provider();
    let texts = vec![
        "first document".to_string(),
        "second document".to_string(),
        "third".to_string(),
    ];
    let vecs = provider.embed(&texts).await?;
    assert_eq!(vecs.len(), 3);
    let dim = cfg_dimension_or_vector_len(&provider, &vecs[0]);
    for v in &vecs {
        assert_eq!(v.len(), dim);
    }
    Ok(())
}

#[tokio::test]
#[ignore]
async fn embedding_dimension_matches_config() -> Result<()> {
    let cfg = load_test_config();
    let expected = cfg.dimension.map(|d| d as usize);
    let provider = make_provider();
    let vecs = provider.embed(&["dimension check".to_string()]).await?;
    let len = vecs[0].len();
    if let Some(d) = expected {
        assert_eq!(len, d);
        assert_eq!(provider.dimension(), d);
    } else {
        assert_eq!(len, provider.dimension());
    }
    Ok(())
}

#[tokio::test]
#[ignore]
async fn embedding_empty_input() -> Result<()> {
    let provider = make_provider();
    let vecs = provider.embed(&[]).await?;
    assert!(vecs.is_empty());
    Ok(())
}

fn cfg_dimension_or_vector_len(provider: &EmbeddingProvider, vector: &[f32]) -> usize {
    let d = provider.dimension();
    if d > 0 {
        d
    } else {
        vector.len()
    }
}
