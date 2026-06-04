package csoss.veda.sdk.model;

import com.fasterxml.jackson.annotation.JsonIgnoreProperties;
import com.fasterxml.jackson.annotation.JsonProperty;

import java.util.Collections;
import java.util.List;

/** Result of {@code POST /v1/vectors/query}. */
@JsonIgnoreProperties(ignoreUnknown = true)
public final class QueryResult {
    @JsonProperty("hits")
    private List<RecordHit> hits;

    public List<RecordHit> getHits() {
        return hits == null ? Collections.<RecordHit>emptyList() : hits;
    }
}
