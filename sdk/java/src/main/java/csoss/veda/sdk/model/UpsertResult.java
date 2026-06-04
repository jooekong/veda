package csoss.veda.sdk.model;

import com.fasterxml.jackson.annotation.JsonIgnoreProperties;
import com.fasterxml.jackson.annotation.JsonProperty;

import java.util.Collections;
import java.util.List;

/** Result of {@code POST /v1/vectors/upsert}. */
@JsonIgnoreProperties(ignoreUnknown = true)
public final class UpsertResult {
    @JsonProperty("ids")
    private List<String> ids;
    @JsonProperty("commit_ts")
    private long commitTs;

    /**
     * Ids actually written, in request order after same-batch dedupe by id
     * (last-wins). May be shorter than the request when duplicate ids were
     * sent. For id-less records this is the only place the server-generated
     * UUID is surfaced — store it to reference the row later.
     */
    public List<String> getIds() {
        return ids == null ? Collections.<String>emptyList() : ids;
    }

    /** Server-local commit time, epoch millis (sufficient for read-your-writes on the same server). */
    public long getCommitTs() {
        return commitTs;
    }
}
