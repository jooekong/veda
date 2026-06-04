package csoss.veda.sdk.model;

import com.fasterxml.jackson.annotation.JsonIgnoreProperties;
import com.fasterxml.jackson.annotation.JsonProperty;
import com.fasterxml.jackson.databind.annotation.JsonDeserialize;
import csoss.veda.sdk.internal.EpochMilliInstantDeserializer;

import java.time.Instant;
import java.util.List;
import java.util.Map;

/**
 * A hit from {@code /v1/vectors/query} (direct lookup by id). Same shape as
 * {@link SearchHit} but with no {@code score} / {@code score_type} — this is an
 * exact-id match, not a ranked result.
 */
@JsonIgnoreProperties(ignoreUnknown = true)
public final class RecordHit {
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

    public Object getMeta() {
        return meta;
    }

    @SuppressWarnings("unchecked")
    public Map<String, Object> metaAsMap() {
        return (meta instanceof Map) ? (Map<String, Object>) meta : null;
    }

    public Instant getCreatedAt() {
        return createdAt;
    }

    public Instant getUpdatedAt() {
        return updatedAt;
    }
}
