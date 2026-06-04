package csoss.veda.sdk.model;

import com.fasterxml.jackson.annotation.JsonValue;

import java.util.Locale;

/**
 * Ranker selection for {@code /v1/vectors/search}.
 *
 * <ul>
 *   <li>{@link #HYBRID} (server default) — dense ANN + BM25 fused by RRF;
 *       {@code score_type=rrf}. Requires a server with sparse-vector support.</li>
 *   <li>{@link #SEMANTIC} — dense ANN over the embedded query; {@code score_type=cosine}.</li>
 *   <li>{@link #FULLTEXT} — BM25 over the tokenized text, skips embedding;
 *       {@code score_type=bm25}.</li>
 * </ul>
 *
 * Scores are <em>not</em> comparable across modes — read {@link SearchHit#getScoreType()}.
 */
public enum SearchMode {
    HYBRID,
    SEMANTIC,
    FULLTEXT;

    @JsonValue
    public String wire() {
        return name().toLowerCase(Locale.ROOT);
    }
}
