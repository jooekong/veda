package csoss.veda.sdk;

import csoss.veda.sdk.error.ErrorCode;
import csoss.veda.sdk.error.VedaApiException;
import csoss.veda.sdk.filter.Filter;
import csoss.veda.sdk.model.DeleteRequest;
import csoss.veda.sdk.model.DeleteResult;
import csoss.veda.sdk.model.QueryRequest;
import csoss.veda.sdk.model.QueryResult;
import csoss.veda.sdk.model.Record;
import csoss.veda.sdk.model.RecordHit;
import csoss.veda.sdk.model.SearchHit;
import csoss.veda.sdk.model.SearchMode;
import csoss.veda.sdk.model.SearchRequest;
import csoss.veda.sdk.model.SearchResult;
import csoss.veda.sdk.model.UpsertRequest;
import csoss.veda.sdk.model.UpsertResult;
import org.junit.jupiter.api.AfterAll;
import org.junit.jupiter.api.BeforeAll;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.condition.EnabledIfEnvironmentVariable;

import java.util.Collections;
import java.util.List;
import java.util.UUID;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

/**
 * Real-server contract test (no mocks) — the SDK's release gate. Hits a live
 * Veda + Milvus + embedding stack. Run manually before tagging an SDK release:
 *
 * <pre>
 *   VEDA_URL=http://10.79.51.161:9009 \
 *   VEDA_API_KEY=vk_... \
 *   VEDA_WS_ID=ws-... \
 *   mvn -f sdk/java/pom.xml verify -P integration
 * </pre>
 *
 * Auto-skips when {@code VEDA_URL} is absent, so it never breaks the default
 * build or a CI runner without access to internal services.
 */
@EnabledIfEnvironmentVariable(named = "VEDA_URL", matches = ".+")
class VedaClientIT {

    private static VedaClient veda;
    private static String runPrefix;

    @BeforeAll
    static void setUp() {
        veda = VedaClient.builder()
                .baseUrl(System.getenv("VEDA_URL"))
                .apiKey(System.getenv("VEDA_API_KEY"))
                .workspaceId(System.getenv("VEDA_WS_ID"))
                .build();
        // Unique id prefix so parallel/repeat runs don't collide in the shared
        // default dataset.
        runPrefix = "javasdk-it-" + UUID.randomUUID().toString().substring(0, 8) + "-";
    }

    @AfterAll
    static void tearDown() {
        if (veda != null) {
            veda.close();
        }
    }

    @Test
    void fullLifecycle() {
        String shoe = runPrefix + "shoe";
        String dup = runPrefix + "dup";

        // Upsert with a same-batch duplicate id to exercise last-wins dedupe.
        UpsertResult up = veda.upsert(UpsertRequest.builder()
                .addRecord(Record.builder().id(shoe).text("Air Jordan 1 basketball sneaker")
                        .category("shoes").tags("sale").meta("price", 1299).build())
                .addRecord(Record.builder().id(dup).text("first version").meta("price", 1).build())
                .addRecord(Record.builder().id(dup).text("Yeezy 350 running shoe").meta("price", 1599).build())
                .build());
        try {
            // 3 records but `dup` appears twice → deduped to 2 ids.
            assertEquals(2, up.getIds().size(), "same-batch duplicate id should be deduped");
            assertTrue(up.getCommitTs() > 0);

            // Search with a meta filter; default-projection hit carries all fields.
            SearchResult found = veda.search(SearchRequest.builder()
                    .query("sneakers under 1500")
                    .topK(10)
                    .filter(Filter.must().lt("meta.price", 1500))
                    .build());
            assertContainsId(found.getHits(), shoe);
            assertScoresDescending(found.getHits());
            for (SearchHit h : found.getHits()) {
                if (shoe.equals(h.getId())) {
                    assertEquals("shoes", h.getCategory());
                    assertNotNull(h.getCreatedAt(), "created_at should deserialize to an Instant");
                    assertEquals("rrf", h.getScoreType(), "default mode is hybrid → rrf");
                }
            }

            // output_fields projection: only id + text on the wire.
            QueryResult q = veda.query(QueryRequest.builder()
                    .ids(shoe, dup).outputFields("text").build());
            assertEquals(2, q.getHits().size());
            for (RecordHit h : q.getHits()) {
                assertNotNull(h.getText());
                // category/meta excluded by projection.
                assertEquals(null, h.getCategory());
                assertEquals(null, h.getMeta());
            }
        } finally {
            DeleteResult del = veda.delete(DeleteRequest.builder().ids(shoe, dup).build());
            // delete_count == number of id terms, regardless of physical existence.
            assertEquals(2, del.getDeleteCount());
        }
    }

    @Test
    void searchModesReportScoreType() {
        String id = runPrefix + "mode";
        veda.upsert(UpsertRequest.builder()
                .addRecord(Record.builder().id(id).text("quantum entanglement primer").build())
                .build());
        try {
            SearchHit s = first(veda.search(SearchRequest.builder()
                    .query("quantum entanglement").mode(SearchMode.SEMANTIC).build()).getHits());
            assertEquals("cosine", s.getScoreType());

            SearchHit f = first(veda.search(SearchRequest.builder()
                    .query("quantum entanglement").mode(SearchMode.FULLTEXT).build()).getHits());
            assertEquals("bm25", f.getScoreType());

            SearchHit h = first(veda.search(SearchRequest.builder()
                    .query("quantum entanglement").mode(SearchMode.HYBRID).build()).getHits());
            assertEquals("rrf", h.getScoreType());
        } finally {
            veda.delete(DeleteRequest.builder().ids(id).build());
        }
    }

    @Test
    void minScoreFiltersSemanticAndRejectsHybrid() {
        String id = runPrefix + "score";
        veda.upsert(UpsertRequest.builder()
                .addRecord(Record.builder().id(id).text("a totally unrelated cooking recipe").build())
                .build());
        try {
            // Very high cosine floor → unrelated doc filtered out.
            SearchResult r = veda.search(SearchRequest.builder()
                    .query("distributed systems consensus protocol")
                    .mode(SearchMode.SEMANTIC).minScore(0.95).build());
            for (SearchHit h : r.getHits()) {
                assertTrue(h.getScore() >= 0.95, "survivors must clear the floor");
            }

            // min_score with hybrid (incl. default) is a 400.
            VedaApiException ex = assertThrows(VedaApiException.class, () -> veda.search(
                    SearchRequest.builder().query("x").mode(SearchMode.HYBRID).minScore(0.4).build()));
            assertEquals(ErrorCode.INVALID_INPUT, ex.getErrorCode());
        } finally {
            veda.delete(DeleteRequest.builder().ids(id).build());
        }
    }

    @Test
    void badTokenIsUnauthorized() {
        try (VedaClient bad = VedaClient.builder()
                .baseUrl(System.getenv("VEDA_URL"))
                .apiKey("vk_definitely_invalid")
                .workspaceId(System.getenv("VEDA_WS_ID"))
                .build()) {
            VedaApiException ex = assertThrows(VedaApiException.class,
                    () -> bad.query(QueryRequest.builder().ids("nope").build()));
            assertEquals(ErrorCode.UNAUTHORIZED, ex.getErrorCode());
            assertEquals(401, ex.getHttpStatus());
        }
    }

    @Test
    void illegalFilterValueIsInvalidInput() {
        // Range op with a boolean value is rejected server-side (bool not ordered).
        VedaApiException ex = assertThrows(VedaApiException.class, () -> veda.search(
                SearchRequest.builder().query("x").filter(Filter.must().gt("meta.flag", true)).build()));
        assertEquals(ErrorCode.INVALID_INPUT, ex.getErrorCode());
    }

    private static SearchHit first(List<SearchHit> hits) {
        assertFalse(hits.isEmpty(), "expected at least one hit");
        return hits.get(0);
    }

    private static void assertContainsId(List<SearchHit> hits, String id) {
        for (SearchHit h : hits) {
            if (id.equals(h.getId())) {
                return;
            }
        }
        throw new AssertionError("expected hits to contain id " + id);
    }

    private static void assertScoresDescending(List<SearchHit> hits) {
        List<SearchHit> hs = Collections.unmodifiableList(hits);
        for (int i = 1; i < hs.size(); i++) {
            assertTrue(hs.get(i - 1).getScore() >= hs.get(i).getScore(),
                    "hits must be in descending score order");
        }
    }
}
