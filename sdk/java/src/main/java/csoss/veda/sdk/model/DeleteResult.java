package csoss.veda.sdk.model;

import com.fasterxml.jackson.annotation.JsonIgnoreProperties;
import com.fasterxml.jackson.annotation.JsonProperty;

/** Result of {@code POST /v1/vectors/delete}. */
@JsonIgnoreProperties(ignoreUnknown = true)
public final class DeleteResult {
    @JsonProperty("delete_count")
    private int deleteCount;

    /**
     * Number of tombstones Milvus created — this <b>always equals
     * {@code ids.length}</b> regardless of whether the rows existed. It is NOT
     * a count of rows actually removed; {@code query} first if you need that.
     */
    public int getDeleteCount() {
        return deleteCount;
    }
}
