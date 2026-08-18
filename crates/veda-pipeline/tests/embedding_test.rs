use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use veda_core::store::EmbeddingService;
use veda_pipeline::embedding::{EmbeddingCache, EmbeddingProvider};
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
    // Required, like the server's own [embedding] section (test.toml.example).
    dimension: u32,
    // airouter/DashScope caps inputs at 10 per request (see test.toml).
    #[serde(default = "default_embed_batch")]
    batch_size: usize,
}

fn default_embed_batch() -> usize {
    10
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
    EmbeddingProvider::new_tuned(
        cfg.api_url,
        cfg.api_key,
        cfg.model,
        cfg.dimension,
        cfg.batch_size,
        8,
    )
    .expect("provider")
}

#[tokio::test]
#[ignore]
async fn embedding_single_text() -> Result<()> {
    let provider = make_provider();
    let vecs = provider
        .embed(&["hello from veda-pipeline".to_string()])
        .await?;
    assert_eq!(vecs.len(), 1);
    assert_eq!(vecs[0].len(), provider.dimension());
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
    for v in &vecs {
        assert_eq!(v.len(), provider.dimension());
    }
    Ok(())
}

#[tokio::test]
#[ignore]
async fn embedding_dimension_matches_config() -> Result<()> {
    let expected = load_test_config().dimension as usize;
    let provider = make_provider();
    let vecs = provider.embed(&["dimension check".to_string()]).await?;
    assert_eq!(vecs[0].len(), expected);
    assert_eq!(provider.dimension(), expected);
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

// ── EmbeddingCache integration tests (Stage 3.3) ────────────────────

/// Wraps a real `EmbeddingProvider` and counts upstream texts embedded.
/// Not a mock — inner is the real HTTP-calling provider; this is just
/// instrumentation to assert cache hits actually skip upstream.
struct CountingProvider {
    inner: EmbeddingProvider,
    upstream_texts: AtomicUsize,
}

#[async_trait]
impl EmbeddingService for CountingProvider {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        self.upstream_texts
            .fetch_add(texts.len(), Ordering::Relaxed);
        self.inner.embed(texts).await
    }

    fn dimension(&self) -> usize {
        self.inner.dimension()
    }
}

#[tokio::test]
#[ignore]
async fn embedding_cache_hit_skips_real_upstream() {
    let cfg = load_test_config();
    let counting = Arc::new(CountingProvider {
        inner: make_provider(),
        upstream_texts: AtomicUsize::new(0),
    });
    let cache = EmbeddingCache::new(counting.clone(), &cfg.model);

    // First call: 1 text → 1 upstream
    let r1 = cache.embed(&["veda cache hit test".into()]).await.unwrap();
    assert_eq!(r1.len(), 1);
    assert_eq!(counting.upstream_texts.load(Ordering::Relaxed), 1);

    // Second call same text: 0 new upstream (cache hit)
    let r2 = cache.embed(&["veda cache hit test".into()]).await.unwrap();
    assert_eq!(r1, r2, "cache should return identical vector");
    assert_eq!(
        counting.upstream_texts.load(Ordering::Relaxed),
        1,
        "second embed should hit cache, not upstream"
    );
}

#[tokio::test]
#[ignore]
async fn embedding_cache_oversize_text_bypasses_cache() {
    let cfg = load_test_config();
    let counting = Arc::new(CountingProvider {
        inner: make_provider(),
        upstream_texts: AtomicUsize::new(0),
    });
    let cache = EmbeddingCache::new(counting.clone(), &cfg.model);

    // 5000 bytes > 4KB cap → bypass cache both times.
    let oversize = "veda oversize ".repeat(360); // ≈ 5040 bytes
    assert!(oversize.len() > 4 * 1024);

    cache.embed(&[oversize.clone()]).await.unwrap();
    assert_eq!(counting.upstream_texts.load(Ordering::Relaxed), 1);

    cache.embed(&[oversize.clone()]).await.unwrap();
    assert_eq!(
        counting.upstream_texts.load(Ordering::Relaxed),
        2,
        "oversize text must not be cached; second call hits upstream"
    );
}

#[tokio::test]
#[ignore]
async fn embedding_cache_batched_partition_with_real_provider() {
    let cfg = load_test_config();
    let counting = Arc::new(CountingProvider {
        inner: make_provider(),
        upstream_texts: AtomicUsize::new(0),
    });
    let cache = EmbeddingCache::new(counting.clone(), &cfg.model);

    // Warm one text
    cache.embed(&["one".into()]).await.unwrap();
    assert_eq!(counting.upstream_texts.load(Ordering::Relaxed), 1);

    // Three texts: "one" hits cache, "two"/"three" miss → single batched call
    let r = cache
        .embed(&["one".into(), "two".into(), "three".into()])
        .await
        .unwrap();
    assert_eq!(r.len(), 3);
    assert_eq!(
        counting.upstream_texts.load(Ordering::Relaxed),
        3,
        "only 2 new texts should hit upstream (batched as one call)"
    );
}
