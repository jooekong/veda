package csoss.veda.sdk.model;

import com.fasterxml.jackson.annotation.JsonInclude;
import com.fasterxml.jackson.annotation.JsonProperty;
import csoss.veda.sdk.filter.Filter;

import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.List;

/**
 * Body for {@code POST /v1/vectors/search}.
 *
 * <p>{@code mode} defaults (server-side) to {@link SearchMode#HYBRID}.
 * {@code minScore} is a relevance floor that only applies to
 * {@code semantic}/{@code fulltext}; sending it with {@code hybrid} (incl. the
 * default mode) is rejected by the server with {@code INVALID_INPUT} — set
 * {@code mode(SearchMode.SEMANTIC)} to use a threshold. {@code mode}/
 * {@code minScore} require a server with sparse-vector support.
 */
@JsonInclude(JsonInclude.Include.NON_NULL)
public final class SearchRequest {
    @JsonProperty("workspace_id")
    private final String workspaceId;
    @JsonProperty("dataset")
    private final String dataset;
    @JsonProperty("query")
    private final String query;
    @JsonProperty("mode")
    private final SearchMode mode;
    @JsonProperty("top_k")
    private final Integer topK;
    @JsonProperty("min_score")
    private final Double minScore;
    @JsonProperty("filter")
    private final Filter filter;
    @JsonProperty("output_fields")
    private final List<String> outputFields;

    private SearchRequest(Builder b) {
        this.workspaceId = b.workspaceId;
        this.dataset = b.dataset;
        this.query = b.query;
        this.mode = b.mode;
        this.topK = b.topK;
        this.minScore = b.minScore;
        this.filter = (b.filter == null || b.filter.isEmpty()) ? null : b.filter;
        this.outputFields = b.outputFields == null ? null
                : Collections.unmodifiableList(new ArrayList<>(b.outputFields));
    }

    public String getWorkspaceId() {
        return workspaceId;
    }

    public String getDataset() {
        return dataset;
    }

    public String getQuery() {
        return query;
    }

    public SearchMode getMode() {
        return mode;
    }

    public Integer getTopK() {
        return topK;
    }

    public Double getMinScore() {
        return minScore;
    }

    public Filter getFilter() {
        return filter;
    }

    public List<String> getOutputFields() {
        return outputFields;
    }

    public static Builder builder() {
        return new Builder();
    }

    public static final class Builder {
        private String workspaceId;
        private String dataset;
        private String query;
        private SearchMode mode;
        private Integer topK;
        private Double minScore;
        private Filter filter;
        private List<String> outputFields;

        public Builder workspaceId(String workspaceId) {
            this.workspaceId = workspaceId;
            return this;
        }

        public Builder dataset(String dataset) {
            this.dataset = dataset;
            return this;
        }

        public Builder query(String query) {
            this.query = query;
            return this;
        }

        public Builder mode(SearchMode mode) {
            this.mode = mode;
            return this;
        }

        public Builder topK(int topK) {
            this.topK = topK;
            return this;
        }

        public Builder minScore(double minScore) {
            this.minScore = minScore;
            return this;
        }

        public Builder filter(Filter filter) {
            this.filter = filter;
            return this;
        }

        public Builder outputFields(List<String> outputFields) {
            this.outputFields = outputFields;
            return this;
        }

        public Builder outputFields(String... outputFields) {
            this.outputFields = outputFields == null ? null : new ArrayList<>(Arrays.asList(outputFields));
            return this;
        }

        public SearchRequest build() {
            if (query == null || query.isEmpty()) {
                throw new IllegalArgumentException("SearchRequest.query is required and must be non-empty");
            }
            return new SearchRequest(this);
        }
    }
}
