package csoss.veda.sdk.model;

import com.fasterxml.jackson.annotation.JsonIgnoreProperties;
import com.fasterxml.jackson.annotation.JsonProperty;

import java.util.Collections;
import java.util.List;

/** Result of {@code POST /v1/vectors/search}: hits in descending score order. */
@JsonIgnoreProperties(ignoreUnknown = true)
public final class SearchResult {
    @JsonProperty("hits")
    private List<SearchHit> hits;

    public List<SearchHit> getHits() {
        return hits == null ? Collections.<SearchHit>emptyList() : hits;
    }
}
