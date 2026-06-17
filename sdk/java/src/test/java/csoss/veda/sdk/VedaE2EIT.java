package csoss.veda.sdk;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.databind.node.ObjectNode;
import csoss.veda.sdk.error.ErrorCode;
import csoss.veda.sdk.error.VedaApiException;
import csoss.veda.sdk.model.DeleteRequest;
import csoss.veda.sdk.model.QueryRequest;
import csoss.veda.sdk.model.QueryResult;
import csoss.veda.sdk.model.Record;
import csoss.veda.sdk.model.SearchHit;
import csoss.veda.sdk.model.SearchMode;
import csoss.veda.sdk.model.SearchRequest;
import csoss.veda.sdk.model.SearchResult;
import csoss.veda.sdk.model.UpsertRequest;
import csoss.veda.sdk.model.UpsertResult;
import okhttp3.MediaType;
import okhttp3.OkHttpClient;
import okhttp3.Request;
import okhttp3.RequestBody;
import okhttp3.Response;
import org.junit.jupiter.api.AfterAll;
import org.junit.jupiter.api.BeforeAll;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.condition.EnabledIfEnvironmentVariable;

import java.io.IOException;
import java.util.List;
import java.util.UUID;
import java.util.concurrent.TimeUnit;
import java.util.function.BooleanSupplier;
import java.util.function.Predicate;
import java.util.stream.Collectors;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

/**
 * Self-contained black-box E2E for the Java SDK — the Java mirror of the
 * server's {@code remote_e2e_test.rs} ({@code db_vectors_*} group). Each test
 * <b>bootstraps its own account + db workspace over raw HTTP</b> (the control
 * plane is outside the SDK's data-plane surface), drives the data plane
 * <b>through the SDK</b>, then best-effort deletes the workspace.
 *
 * <p>Differences from {@link VedaClientIT} (and why this exists):
 * <ul>
 *   <li>Needs only {@code VEDA_BASE_URL} — it provisions account/workspace
 *       itself, so it runs against any live deployment without a pre-seeded
 *       {@code VEDA_WS_ID}, exactly like the Rust e2e suite.</li>
 *   <li><b>Polls</b> search after upsert to ride out Milvus visibility lag (the
 *       roundtrip in {@code VedaClientIT} can flake without this).</li>
 *   <li><b>Proves</b> the sparse/dense/hybrid legs really fire with the server
 *       suite's orthogonal-document trick: a rare token isolates one doc under
 *       BM25, and a zero-overlap paraphrase exercises the dense leg — not just
 *       asserting the {@code score_type} label.</li>
 * </ul>
 *
 * <pre>
 *   VEDA_BASE_URL=https://veda.dbpaas.dingdongxiaoqu.com \
 *   mvn -f sdk/java/pom.xml verify -P integration
 * </pre>
 *
 * Auto-skips when {@code VEDA_BASE_URL} is absent, so it never breaks the
 * default build or a CI runner without access to a live server.
 */
@EnabledIfEnvironmentVariable(named = "VEDA_BASE_URL", matches = ".+")
class VedaE2EIT {

    private static final MediaType JSON = MediaType.parse("application/json; charset=utf-8");
    /** Generous ceiling for Milvus to index a fresh upsert before search sees it. */
    private static final long VISIBILITY_TIMEOUT_MS = 25_000L;
    private static final long POLL_INTERVAL_MS = 700L;

    private static String baseUrl;
    private static OkHttpClient bootstrapHttp;
    private static ObjectMapper mapper;
    /** A vk_ account key minted once; each test creates its own workspace under it. */
    private static String accountKey;

    @BeforeAll
    static void setUp() throws IOException {
        baseUrl = stripTrailingSlash(System.getenv("VEDA_BASE_URL"));
        mapper = new ObjectMapper();
        bootstrapHttp = new OkHttpClient.Builder()
                .connectTimeout(10, TimeUnit.SECONDS)
                .readTimeout(30, TimeUnit.SECONDS)
                .writeTimeout(30, TimeUnit.SECONDS)
                .build();
        accountKey = createAccount();
    }

    @AfterAll
    static void tearDown() {
        if (bootstrapHttp != null) {
            bootstrapHttp.dispatcher().executorService().shutdown();
            bootstrapHttp.connectionPool().evictAll();
        }
    }

    /** upsert (explicit + auto id) → search → query → delete, all through the SDK. */
    @Test
    void fullRoundtrip() {
        String ws = newDbWorkspace();
        try (VedaClient veda = clientFor(ws)) {
            UpsertResult up = veda.upsert(UpsertRequest.builder()
                    .addRecord(Record.builder().id("r1").text("the quick brown fox jumps")
                            .category("animals").tags("fox").meta("legs", 4).build())
                    .addRecord(Record.builder().id("r2").text("a lazy dog sleeps all day")
                            .category("animals").tags("dog").meta("legs", 4).build())
                    .addRecord(Record.builder().text("vector databases power semantic search")
                            .category("tech").build()) // id omitted → server-generated UUID
                    .build());
            assertEquals(3, up.getIds().size(), "three ids (2 explicit + 1 auto)");
            assertTrue(up.getCommitTs() > 0, "commit_ts present");

            // Default mode is hybrid; the fox doc ranks first. Poll out Milvus lag.
            SearchRequest foxQ = SearchRequest.builder().query("quick brown fox").topK(10).build();
            List<SearchHit> hits = pollHits(veda, foxQ, hs -> !hs.isEmpty() && "r1".equals(hs.get(0).getId()));
            SearchHit top = hits.get(0);
            assertEquals("r1", top.getId(), "fox ranks first");
            assertEquals("animals", top.getCategory());
            assertEquals("fox", top.getTags().get(0));
            assertTrue(top.getText().contains("fox"));
            assertEquals(4, ((Number) top.metaAsMap().get("legs")).intValue(), "meta round-trips");
            assertNotNull(top.getCreatedAt(), "created_at deserializes to Instant");
            assertEquals("rrf", top.getScoreType(), "default mode is hybrid → rrf");

            // Query by id: direct lookup, missing id silently absent.
            QueryResult q = veda.query(QueryRequest.builder().ids("r1", "r2").build());
            assertEquals(2, q.getHits().size(), "two records by id");
            assertTrue(veda.query(QueryRequest.builder().ids("does-not-exist").build()).getHits().isEmpty(),
                    "missing id yields no hit, no error");

            // delete_count mirrors the id list, regardless of physical existence.
            assertEquals(1, veda.delete(DeleteRequest.builder().ids("r1").build()).getDeleteCount());
            assertEquals(1, veda.delete(DeleteRequest.builder().ids("never-existed").build()).getDeleteCount(),
                    "delete_count == len(ids) regardless of existence (tombstone model)");

            // After the tombstone is visible, r1 is gone.
            assertTrue(poll(() -> veda.query(QueryRequest.builder().ids("r1").build()).getHits().isEmpty()),
                    "deleted record still queryable after timeout");
        } finally {
            dropWorkspace(ws);
        }
    }

    /** Dense {@code semantic} matches on MEANING: each query shares zero tokens with its target. */
    @Test
    void denseSemanticMatchesByMeaning() {
        String ws = newDbWorkspace();
        try (VedaClient veda = clientFor(ws)) {
            veda.upsert(UpsertRequest.builder()
                    .addRecord(Record.builder().id("pet").text("a domestic feline dozed on the warm windowsill").build())
                    .addRecord(Record.builder().id("auto").text("the mechanic replaced the engine timing belt").build())
                    .build());

            // "kitten napping" ≈ feline/dozed, with no shared tokens.
            SearchRequest catQ = SearchRequest.builder().query("a kitten taking a nap").mode(SearchMode.SEMANTIC).topK(10).build();
            List<SearchHit> cat = pollHits(veda, catQ, hs -> !hs.isEmpty() && "pet".equals(hs.get(0).getId()));
            assertEquals("pet", cat.get(0).getId(), "dense ranks the cat record first");
            assertEquals("cosine", cat.get(0).getScoreType(), "semantic score_type");

            // "fixing a broken car" ≈ the auto record, again no shared tokens.
            SearchRequest carQ = SearchRequest.builder().query("fixing a broken car").mode(SearchMode.SEMANTIC).topK(10).build();
            List<SearchHit> car = pollHits(veda, carQ, hs -> !hs.isEmpty() && "auto".equals(hs.get(0).getId()));
            assertEquals("auto", car.get(0).getId(), "dense ranks the car record first");
        } finally {
            dropWorkspace(ws);
        }
    }

    /**
     * Proves the sparse leg really fires: two topically orthogonal docs, a rare
     * token isolates each under {@code fulltext} BM25 (score_type=bm25), and
     * {@code hybrid} RRF (score_type=rrf) both tops the token doc and still
     * retrieves the other via a zero-overlap paraphrase (the dense leg).
     */
    @Test
    void fulltextAndHybridIsolateByRareToken() {
        String ws = newDbWorkspace();
        try (VedaClient veda = clientFor(ws)) {
            veda.upsert(UpsertRequest.builder()
                    .addRecord(Record.builder().id("music")
                            .text("the xylophone quokka performs midnight marshmallow concerts in the grove").build())
                    .addRecord(Record.builder().id("finance")
                            .text("quarterly revenue guidance was revised upward after strong fiscal results").build())
                    .build());

            // Fulltext / BM25: the rare token resolves ONLY its doc.
            SearchRequest ftMarsh = SearchRequest.builder().query("marshmallow").mode(SearchMode.FULLTEXT).topK(10).build();
            List<SearchHit> ft = pollHits(veda, ftMarsh, hs -> ids(hs).contains("music"));
            assertTrue(ids(ft).contains("music"), "BM25 finds the token doc");
            assertFalse(ids(ft).contains("finance"), "BM25 excludes the doc lacking the term");
            assertEquals("bm25", ft.get(0).getScoreType(), "fulltext score_type");

            // The other rare token isolates the other doc.
            SearchRequest ftRev = SearchRequest.builder().query("revenue").mode(SearchMode.FULLTEXT).topK(10).build();
            List<SearchHit> ft2 = pollHits(veda, ftRev, hs -> ids(hs).contains("finance"));
            assertTrue(ids(ft2).contains("finance") && !ids(ft2).contains("music"), "BM25 isolates finance");

            // Hybrid / RRF: rare token still ranks its doc top.
            SearchRequest hyMarsh = SearchRequest.builder().query("marshmallow concerts").mode(SearchMode.HYBRID).topK(10).build();
            List<SearchHit> hy = pollHits(veda, hyMarsh, hs -> !hs.isEmpty() && "music".equals(hs.get(0).getId()));
            assertEquals("music", hy.get(0).getId(), "hybrid ranks the token doc first");
            assertEquals("rrf", hy.get(0).getScoreType(), "hybrid score_type");

            // Hybrid dense leg: a zero-shared-token paraphrase still retrieves finance.
            SearchRequest hyPara = SearchRequest.builder()
                    .query("corporate profit expectations for the period").mode(SearchMode.HYBRID).topK(10).build();
            List<SearchHit> hy2 = pollHits(veda, hyPara, hs -> ids(hs).contains("finance"));
            assertTrue(ids(hy2).contains("finance"), "hybrid dense leg retrieves finance via paraphrase");
            assertEquals("rrf", hy2.get(0).getScoreType(), "hybrid score_type (dense-leaning query)");
        } finally {
            dropWorkspace(ws);
        }
    }

    /**
     * {@code min_score} relevance floor over the wire: prunes the weakly-related
     * doc on {@code semantic} (a model-independent floor = midpoint of the two
     * live scores), and {@code hybrid} + {@code min_score} is rejected 400.
     */
    @Test
    void minScoreFloorAndHybridRejected() {
        String ws = newDbWorkspace();
        try (VedaClient veda = clientFor(ws)) {
            veda.upsert(UpsertRequest.builder()
                    .addRecord(Record.builder().id("near")
                            .text("ocean tides rise and fall with the gravitational pull of the moon").build())
                    .addRecord(Record.builder().id("far")
                            .text("the accountant reconciled the quarterly tax spreadsheet").build())
                    .build());

            // Baseline: wait for both docs, then derive the floor as the midpoint
            // of their live scores (near outranks far → midpoint keeps near, drops
            // far). Avoids a hardcoded threshold that flakes across embedding models.
            SearchRequest base = SearchRequest.builder().query("ocean tides and waves").mode(SearchMode.SEMANTIC).topK(10).build();
            List<SearchHit> both = pollHits(veda, base, hs -> ids(hs).contains("near") && ids(hs).contains("far"));
            assertTrue(ids(both).contains("near") && ids(both).contains("far"), "both docs indexed within timeout");
            double nearScore = scoreOf(both, "near");
            double farScore = scoreOf(both, "far");
            assertTrue(nearScore > farScore, "near must outrank far: near=" + nearScore + " far=" + farScore);
            double floor = (nearScore + farScore) / 2.0;

            SearchResult floored = veda.search(SearchRequest.builder()
                    .query("ocean tides and waves").mode(SearchMode.SEMANTIC).topK(10).minScore(floor).build());
            List<String> got = ids(floored.getHits());
            assertTrue(got.contains("near"), "strong doc survives the floor: " + got);
            assertFalse(got.contains("far"), "weak doc filtered by the floor: " + got);
            assertTrue(floored.getHits().stream().allMatch(h -> h.getScore() >= floor), "all hits clear the floor");

            // hybrid + min_score → 400 INVALID_INPUT (RRF is a rank artifact, not relevance).
            VedaApiException ex = assertThrows(VedaApiException.class, () -> veda.search(
                    SearchRequest.builder().query("ocean").mode(SearchMode.HYBRID).minScore(0.4).build()));
            assertEquals(ErrorCode.INVALID_INPUT, ex.getErrorCode(), "hybrid + min_score rejected");
        } finally {
            dropWorkspace(ws);
        }
    }

    // ── bootstrap (raw HTTP — control plane is outside the SDK surface) ──────

    private static VedaClient clientFor(String workspaceId) {
        // Data-plane no longer accepts the vk_ account key — it requires a wk_
        // workspace key. Mint one for this workspace and use it.
        return VedaClient.builder().baseUrl(baseUrl).apiKey(issueWorkspaceKey(workspaceId))
                .workspaceId(workspaceId).build();
    }

    /** Issues a readwrite wk_ workspace key under the account; the data plane needs this, not vk_. */
    private static String issueWorkspaceKey(String workspaceId) {
        try {
            ObjectNode body = mapper.createObjectNode();
            body.put("name", "e2e-key");
            JsonNode data = postData("/v1/workspaces/" + workspaceId + "/keys", accountKey, body);
            return data.get("key").asText();
        } catch (IOException e) {
            throw new IllegalStateException("failed to issue workspace key for " + workspaceId, e);
        }
    }

    private static String createAccount() throws IOException {
        ObjectNode body = mapper.createObjectNode();
        body.put("name", "e2e");
        body.put("email", "e2e-" + UUID.randomUUID().toString().replace("-", "") + "@example.com");
        body.put("password", "pass1234");
        return postData("/v1/accounts", null, body).get("api_key").asText();
    }

    private static String newDbWorkspace() {
        try {
            ObjectNode body = mapper.createObjectNode();
            body.put("name", "ws-db-" + UUID.randomUUID().toString().replace("-", ""));
            body.put("kind", "db");
            JsonNode data = postData("/v1/workspaces", accountKey, body);
            assertEquals("db", data.get("kind").asText(), "workspace kind echoes request");
            return data.get("id").asText();
        } catch (IOException e) {
            throw new IllegalStateException("failed to bootstrap db workspace", e);
        }
    }

    /** Best-effort teardown; a leftover is a unique empty workspace (no delete-account API). */
    private static void dropWorkspace(String ws) {
        Request req = new Request.Builder().url(baseUrl + "/v1/workspaces/" + ws)
                .delete().header("Authorization", "Bearer " + accountKey).build();
        try (Response ignored = bootstrapHttp.newCall(req).execute()) {
            // ignore result
        } catch (IOException ignored) {
            // ignore
        }
    }

    private static JsonNode postData(String path, String bearer, Object body) throws IOException {
        byte[] payload = mapper.writeValueAsBytes(body);
        Request.Builder rb = new Request.Builder().url(baseUrl + path).post(RequestBody.create(payload, JSON));
        if (bearer != null) {
            rb.header("Authorization", "Bearer " + bearer);
        }
        try (Response resp = bootstrapHttp.newCall(rb.build()).execute()) {
            byte[] bytes = resp.body() == null ? new byte[0] : resp.body().bytes();
            JsonNode root = mapper.readTree(bytes);
            if (root == null || !root.path("success").asBoolean(false)) {
                throw new IOException("bootstrap " + path + " failed (HTTP " + resp.code() + "): " + new String(bytes));
            }
            return root.get("data");
        }
    }

    // ── polling (Milvus visibility lag) + small helpers ─────────────────────

    /** Re-runs the search until {@code done} holds or the timeout elapses; returns the last hits. */
    private static List<SearchHit> pollHits(VedaClient veda, SearchRequest req, Predicate<List<SearchHit>> done) {
        long start = System.currentTimeMillis();
        while (true) {
            List<SearchHit> hits = veda.search(req).getHits();
            if (done.test(hits) || System.currentTimeMillis() - start >= VISIBILITY_TIMEOUT_MS) {
                return hits;
            }
            sleep(POLL_INTERVAL_MS);
        }
    }

    private static boolean poll(BooleanSupplier cond) {
        long start = System.currentTimeMillis();
        while (true) {
            if (cond.getAsBoolean()) {
                return true;
            }
            if (System.currentTimeMillis() - start >= VISIBILITY_TIMEOUT_MS) {
                return false;
            }
            sleep(POLL_INTERVAL_MS);
        }
    }

    private static List<String> ids(List<SearchHit> hits) {
        return hits.stream().map(SearchHit::getId).collect(Collectors.toList());
    }

    private static double scoreOf(List<SearchHit> hits, String id) {
        return hits.stream().filter(h -> id.equals(h.getId())).findFirst()
                .map(SearchHit::getScore).orElseThrow(() -> new AssertionError("missing hit: " + id));
    }

    private static String stripTrailingSlash(String s) {
        String r = s;
        while (r.endsWith("/")) {
            r = r.substring(0, r.length() - 1);
        }
        return r;
    }

    private static void sleep(long ms) {
        try {
            Thread.sleep(ms);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
            throw new IllegalStateException("interrupted during poll", e);
        }
    }
}
