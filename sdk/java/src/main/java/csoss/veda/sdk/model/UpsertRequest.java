package csoss.veda.sdk.model;

import com.fasterxml.jackson.annotation.JsonInclude;
import com.fasterxml.jackson.annotation.JsonProperty;

import java.util.ArrayList;
import java.util.Collections;
import java.util.List;

/**
 * Body for {@code POST /v1/vectors/upsert}. {@code workspaceId} / {@code dataset}
 * are optional per-request overrides; when unset the client's defaults are used
 * (and {@code dataset} ultimately falls back to the server's {@code default}).
 * At most 500 records per call.
 */
@JsonInclude(JsonInclude.Include.NON_NULL)
public final class UpsertRequest {
    @JsonProperty("workspace_id")
    private final String workspaceId;
    @JsonProperty("dataset")
    private final String dataset;
    @JsonProperty("records")
    private final List<Record> records;

    private UpsertRequest(Builder b) {
        this.workspaceId = b.workspaceId;
        this.dataset = b.dataset;
        this.records = Collections.unmodifiableList(new ArrayList<>(b.records));
    }

    public String getWorkspaceId() {
        return workspaceId;
    }

    public String getDataset() {
        return dataset;
    }

    public List<Record> getRecords() {
        return records;
    }

    /** True iff every record carries a caller-supplied id (so the batch is idempotent). */
    public boolean isIdempotent() {
        for (Record r : records) {
            if (!r.hasId()) {
                return false;
            }
        }
        return true;
    }

    public static Builder builder() {
        return new Builder();
    }

    public static final class Builder {
        private String workspaceId;
        private String dataset;
        private final List<Record> records = new ArrayList<>();

        public Builder workspaceId(String workspaceId) {
            this.workspaceId = workspaceId;
            return this;
        }

        public Builder dataset(String dataset) {
            this.dataset = dataset;
            return this;
        }

        public Builder addRecord(Record record) {
            if (record == null) {
                throw new IllegalArgumentException("record must not be null");
            }
            this.records.add(record);
            return this;
        }

        public Builder records(List<Record> records) {
            this.records.clear();
            if (records != null) {
                this.records.addAll(records);
            }
            return this;
        }

        public UpsertRequest build() {
            return new UpsertRequest(this);
        }
    }
}
