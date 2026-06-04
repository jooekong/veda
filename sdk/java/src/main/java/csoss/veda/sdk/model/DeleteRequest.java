package csoss.veda.sdk.model;

import com.fasterxml.jackson.annotation.JsonInclude;
import com.fasterxml.jackson.annotation.JsonProperty;

import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.List;

/**
 * Body for {@code POST /v1/vectors/delete} (hard delete by id). At most 500 ids
 * per call. Deleting is idempotent, so the SDK auto-retries it.
 */
@JsonInclude(JsonInclude.Include.NON_NULL)
public final class DeleteRequest {
    @JsonProperty("workspace_id")
    private final String workspaceId;
    @JsonProperty("dataset")
    private final String dataset;
    @JsonProperty("ids")
    private final List<String> ids;

    private DeleteRequest(Builder b) {
        this.workspaceId = b.workspaceId;
        this.dataset = b.dataset;
        this.ids = Collections.unmodifiableList(new ArrayList<>(b.ids));
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

    public static Builder builder() {
        return new Builder();
    }

    public static final class Builder {
        private String workspaceId;
        private String dataset;
        private final List<String> ids = new ArrayList<>();

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

        public DeleteRequest build() {
            return new DeleteRequest(this);
        }
    }
}
