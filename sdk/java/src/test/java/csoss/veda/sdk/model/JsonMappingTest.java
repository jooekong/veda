package csoss.veda.sdk.model;

import com.fasterxml.jackson.databind.JsonNode;
import csoss.veda.sdk.filter.Filter;
import csoss.veda.sdk.internal.Json;
import org.junit.jupiter.api.Test;

import java.time.Instant;
import java.util.Map;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertTrue;

class JsonMappingTest {

    @Test
    void recordOmitsNullIdAndDefaults() {
        Record r = Record.builder().text("Air Jordan 1").meta("price", 1299).build();
        JsonNode node = Json.MAPPER.valueToTree(r);

        assertFalse(node.has("id"), "null id must be omitted so the server mints a UUID");
        assertFalse(node.has("category"));
        assertEquals("Air Jordan 1", node.get("text").asText());
        assertEquals(1299, node.get("meta").get("price").asInt());
    }

    @Test
    void searchRequestUsesSnakeCaseAndModeWire() {
        SearchRequest req = SearchRequest.builder()
                .query("sneakers")
                .mode(SearchMode.SEMANTIC)
                .topK(5)
                .minScore(0.4)
                .outputFields("text", "meta")
                .filter(Filter.must().eq("meta.category", "shoes"))
                .build();
        JsonNode node = Json.MAPPER.valueToTree(req);

        assertEquals("semantic", node.get("mode").asText());
        assertEquals(5, node.get("top_k").asInt());
        assertEquals(0.4, node.get("min_score").asDouble(), 1e-9);
        assertTrue(node.get("output_fields").isArray());
        assertEquals("meta.category", node.get("filter").get("must").get(0).get("field").asText());
    }

    @Test
    void emptyFilterIsDroppedFromSearchRequest() {
        SearchRequest req = SearchRequest.builder().query("x").filter(Filter.must()).build();
        JsonNode node = Json.MAPPER.valueToTree(req);
        assertFalse(node.has("filter"), "an empty filter must not be serialized");
    }

    @Test
    void searchHitParsesEpochMillisAndScoreType() throws Exception {
        String json = "{\"id\":\"sku-1\",\"dataset\":\"products\",\"text\":\"shoe\","
                + "\"meta\":{\"price\":1299},\"created_at\":1735689600000,"
                + "\"updated_at\":1735689600000,\"score\":0.87,\"score_type\":\"cosine\"}";
        SearchHit hit = Json.MAPPER.readValue(json, SearchHit.class);

        assertEquals("sku-1", hit.getId());
        assertEquals(Instant.ofEpochMilli(1735689600000L), hit.getCreatedAt());
        assertEquals(0.87, hit.getScore(), 1e-9);
        assertEquals("cosine", hit.getScoreType());
        Map<String, Object> meta = hit.metaAsMap();
        assertEquals(1299, ((Number) meta.get("price")).intValue());
    }

    @Test
    void searchHitDefaultsScoreTypeToCosineWhenAbsent() throws Exception {
        SearchHit hit = Json.MAPPER.readValue("{\"id\":\"a\",\"score\":0.1}", SearchHit.class);
        assertEquals("cosine", hit.getScoreType());
    }

    @Test
    void responsesIgnoreUnknownFieldsForForwardCompat() throws Exception {
        String json = "{\"id\":\"a\",\"score\":0.1,\"some_future_field\":42,\"nested\":{\"x\":1}}";
        SearchHit hit = Json.MAPPER.readValue(json, SearchHit.class);
        assertEquals("a", hit.getId());
    }

    @Test
    void searchHitToleratesNonObjectMeta() throws Exception {
        SearchHit hit = Json.MAPPER.readValue("{\"id\":\"a\",\"score\":0.1,\"meta\":\"legacy-string\"}", SearchHit.class);
        assertEquals("legacy-string", hit.getMeta());
        assertNull(hit.metaAsMap(), "non-object meta returns null from metaAsMap");
    }

    @Test
    void upsertAndDeleteResultsParse() throws Exception {
        UpsertResult up = Json.MAPPER.readValue("{\"ids\":[\"sku-1\"],\"commit_ts\":1735689600000}", UpsertResult.class);
        assertEquals(1, up.getIds().size());
        assertEquals(1735689600000L, up.getCommitTs());

        DeleteResult del = Json.MAPPER.readValue("{\"delete_count\":2}", DeleteResult.class);
        assertEquals(2, del.getDeleteCount());
    }
}
