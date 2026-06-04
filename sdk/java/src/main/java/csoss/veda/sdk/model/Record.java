package csoss.veda.sdk.model;

import com.fasterxml.jackson.annotation.JsonInclude;
import com.fasterxml.jackson.annotation.JsonProperty;

import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

/**
 * One row to upsert. {@code text} is the only required field; everything else
 * has a server-side default ({@code category="default"}, {@code tags=[]},
 * {@code meta={}}).
 *
 * <p><b>Idempotency:</b> supply your own {@code id} (content hash or a
 * client-generated UUID) if the caller might retry. Omitting {@code id} makes
 * the server mint a fresh UUID on every write — non-idempotent, so the SDK
 * will refuse to auto-retry an upsert batch that contains any id-less record
 * (see {@code VedaClient.upsert}).
 *
 * <p>{@code meta} is intentionally narrowed to a {@code Map<String,Object>} on
 * write (D-f): the server stores arbitrary JSON, but only object-shaped meta
 * can be referenced by the {@code meta.<key>} filter DSL.
 */
@JsonInclude(JsonInclude.Include.NON_NULL)
public final class Record {
    @JsonProperty("id")
    private final String id;
    @JsonProperty("text")
    private final String text;
    @JsonProperty("category")
    private final String category;
    @JsonProperty("tags")
    private final List<String> tags;
    @JsonProperty("meta")
    private final Map<String, Object> meta;

    private Record(Builder b) {
        this.id = b.id;
        this.text = b.text;
        this.category = b.category;
        this.tags = b.tags == null ? null : Collections.unmodifiableList(new ArrayList<>(b.tags));
        this.meta = b.meta == null ? null : Collections.unmodifiableMap(new LinkedHashMap<>(b.meta));
    }

    public String getId() {
        return id;
    }

    public String getText() {
        return text;
    }

    public String getCategory() {
        return category;
    }

    public List<String> getTags() {
        return tags;
    }

    public Map<String, Object> getMeta() {
        return meta;
    }

    /** True when this record carries a caller-supplied id (idempotent upsert). */
    public boolean hasId() {
        return id != null && !id.isEmpty();
    }

    public static Builder builder() {
        return new Builder();
    }

    public static final class Builder {
        private String id;
        private String text;
        private String category;
        private List<String> tags;
        private Map<String, Object> meta;

        public Builder id(String id) {
            this.id = id;
            return this;
        }

        public Builder text(String text) {
            this.text = text;
            return this;
        }

        public Builder category(String category) {
            this.category = category;
            return this;
        }

        public Builder tags(List<String> tags) {
            this.tags = tags;
            return this;
        }

        public Builder tags(String... tags) {
            this.tags = tags == null ? null : new ArrayList<>(Arrays.asList(tags));
            return this;
        }

        public Builder meta(Map<String, Object> meta) {
            this.meta = meta;
            return this;
        }

        /** Adds a single key/value to {@code meta}, creating the map if needed. */
        public Builder meta(String key, Object value) {
            if (this.meta == null) {
                this.meta = new LinkedHashMap<>();
            }
            this.meta.put(key, value);
            return this;
        }

        public Record build() {
            if (text == null || text.isEmpty()) {
                throw new IllegalArgumentException("Record.text is required and must be non-empty");
            }
            return new Record(this);
        }
    }
}
