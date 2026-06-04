package csoss.veda.sdk.model;

import com.fasterxml.jackson.annotation.JsonInclude;
import com.fasterxml.jackson.annotation.JsonProperty;

import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.List;

/**
 * Body for {@code POST /v1/vectors/query} (direct lookup by id). Unknown ids
 * are silently skipped; order is not guaranteed. At most 500 ids per call.
 */
@JsonInclude(JsonInclude.Include.NON_NULL)
public final class QueryRequest {
    @JsonProperty("workspace_id")
    private final String workspaceId;
    @JsonProperty("dataset")
    private final String dataset;
    @JsonProperty("ids")
    private final List<String> ids;
    @JsonProperty("output_fields")
    private final List<String> outputFields;

    private QueryRequest(Builder b) {
        this.workspaceId = b.workspaceId;
        this.dataset = b.dataset;
        this.ids = Collections.unmodifiableList(new ArrayList<>(b.ids));
        this.outputFields = b.outputFields == null ? null
                : Collections.unmodifiableList(new ArrayList<>(b.outputFields));
    }

    public String getWorkspaceId() {
        return workspaceId;
    }

    public String getDataset() {
        return dataset;
    }

    public List<String> getIds() {
        return ids;
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
        private final List<String> ids = new ArrayList<>();
        private List<String> outputFields;

        public Builder workspaceId(String workspaceId) {
            this.workspaceId = workspaceId;
            return this;
        }

        public Builder dataset(String dataset) {
            this.dataset = dataset;
            return this;
        }

        public Builder ids(String... ids) {
            this.ids.clear();
            if (ids != null) {
                this.ids.addAll(Arrays.asList(ids));
            }
            return this;
        }

        public Builder ids(List<String> ids) {
            this.ids.clear();
            if (ids != null) {
                this.ids.addAll(ids);
            }
            return this;
        }

        public Builder outputFields(String... outputFields) {
            this.outputFields = outputFields == null ? null : new ArrayList<>(Arrays.asList(outputFields));
            return this;
        }

        public Builder outputFields(List<String> outputFields) {
            this.outputFields = outputFields;
            return this;
        }

        public QueryRequest build() {
            return new QueryRequest(this);
        }
    }
}
