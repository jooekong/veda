package csoss.veda.sdk.model;

import com.fasterxml.jackson.annotation.JsonIgnoreProperties;
import com.fasterxml.jackson.annotation.JsonProperty;
import com.fasterxml.jackson.databind.annotation.JsonDeserialize;
import csoss.veda.sdk.internal.EpochMilliInstantDeserializer;

import java.time.Instant;
import java.util.List;

/**
 * A ranked hit from {@code /v1/vectors/search}.
 *
 * <p>Projected fields ({@code dataset}/{@code category}/{@code tags}/{@code text}/
 * {@code meta}/{@code created_at}/{@code updated_at}) may be {@code null} when the
 * request used {@code output_fields} to exclude them. {@code id}, {@code score}
 * and {@code score_type} are always present.
 *
 * <p>{@code meta} is exposed as a tolerant {@link Object} (Jackson maps a JSON
 * object to {@code Map<String,Object>}, an array to {@code List}, scalars to
 * boxed primitives) so historical non-object meta values never break
 * deserialization. {@link #metaAsMap()} is a convenience for the common case.
 */
@JsonIgnoreProperties(ignoreUnknown = true)
public final class SearchHit {
    @JsonProperty("id")
    private String id;
    @JsonProperty("dataset")
    private String dataset;
    @JsonProperty("category")
    private String category;
    @JsonProperty("tags")
    private List<String> tags;
    @JsonProperty("text")
    private String text;
    @JsonProperty("meta")
    private Object meta;
    @JsonProperty("created_at")
    @JsonDeserialize(using = EpochMilliInstantDeserializer.class)
    private Instant createdAt;
    @JsonProperty("updated_at")
    @JsonDeserialize(using = EpochMilliInstantDeserializer.class)
    private Instant updatedAt;
    @JsonProperty("score")
    private double score;
    @JsonProperty("score_type")
    private String scoreType;

    public String getId() {
        return id;
    }

    public String getDataset() {
        return dataset;
    }

    public String getCategory() {
        return category;
    }

    public List<String> getTags() {
        return tags;
    }

    public String getText() {
        return text;
    }

    /** Raw meta value (object/array/scalar/null). See {@link #metaAsMap()}. */
    public Object getMeta() {
        return meta;
    }

    /**
     * {@code meta} as a {@code Map<String,Object>} when it is object-shaped,
     * else {@code null}. Object meta is the supported v0 shape (only object
     * meta is filterable).
     */
    @SuppressWarnings("unchecked")
    public java.util.Map<String, Object> metaAsMap() {
        return (meta instanceof java.util.Map) ? (java.util.Map<String, Object>) meta : null;
    }

    public Instant getCreatedAt() {
        return createdAt;
    }

    public Instant getUpdatedAt() {
        return updatedAt;
    }

    /** Relevance score; higher is more relevant. Interpret via {@link #getScoreType()}. */
    public double getScore() {
        return score;
    }

    /**
     * What {@link #getScore()} means: {@code "cosine"} (~[0,1]),
     * {@code "bm25"} (~[0,30+]) or {@code "rrf"} (~[0,0.033]). Scores are NOT
     * comparable across types. Defaults to {@code "cosine"} for a pre-mode
     * server that omits it.
     */
    public String getScoreType() {
        return scoreType == null ? "cosine" : scoreType;
    }
}
