package csoss.veda.sdk;

import com.csoss.monitor.api.trace.Span;
import com.fasterxml.jackson.databind.node.ObjectNode;
import csoss.veda.sdk.internal.HttpExecutor;
import csoss.veda.sdk.internal.Json;
import csoss.veda.sdk.internal.Telemetry;
import csoss.veda.sdk.model.DeleteRequest;
import csoss.veda.sdk.model.DeleteResult;
import csoss.veda.sdk.model.QueryRequest;
import csoss.veda.sdk.model.QueryResult;
import csoss.veda.sdk.model.SearchMode;
import csoss.veda.sdk.model.SearchRequest;
import csoss.veda.sdk.model.SearchResult;
import csoss.veda.sdk.model.UpsertRequest;
import csoss.veda.sdk.model.UpsertResult;
import okhttp3.OkHttpClient;

import java.io.Closeable;

/**
 * Client for the Veda db-workspace vector data-plane: {@code upsert},
 * {@code search}, {@code query}, {@code delete}.
 *
 * <p>Construct via {@link #builder()}. The instance is thread-safe and should
 * be shared (it owns an OkHttp connection pool); {@link #close()} when done.
 *
 * <pre>{@code
 * try (VedaClient veda = VedaClient.builder()
 *         .baseUrl("https://veda.ddmc-inc.com")
 *         .apiKey("vk_...")
 *         .workspaceId("ws-...")
 *         .build()) {
 *     veda.upsert(UpsertRequest.builder()
 *         .addRecord(Record.builder().id("sku-1").text("Air Jordan 1").build())
 *         .build());
 * }
 * }</pre>
 *
 * <p>Marker for the API contract this SDK targets.
 */
public final class VedaClient implements Closeable {
    /** db data-plane API version this SDK targets (server &ge; veda 0.1.x). */
    public static final String SUPPORTED_API = "v0";

    private static final int MAX_BATCH = 500;
    private static final int MAX_TOP_K = 100;

    private final OkHttpClient http;
    private final HttpExecutor executor;
    private final Telemetry telemetry;
    private final String defaultWorkspaceId;
    private final String defaultDataset;

    VedaClient(OkHttpClient http, HttpExecutor executor, Telemetry telemetry,
               String defaultWorkspaceId, String defaultDataset) {
        this.http = http;
        this.executor = executor;
        this.telemetry = telemetry;
        this.defaultWorkspaceId = defaultWorkspaceId;
        this.defaultDataset = defaultDataset;
    }

    public static VedaClientBuilder builder() {
        return new VedaClientBuilder();
    }

    /**
     * Insert or replace records by {@code (dataset, id)}. Auto-retried only when
     * every record carries a caller-supplied id (idempotent); a batch with any
     * id-less record is sent exactly once.
     */
    public UpsertResult upsert(UpsertRequest req) {
        int n = req.getRecords().size();
        if (n == 0) {
            throw new IllegalArgumentException("upsert requires at least one record");
        }
        if (n > MAX_BATCH) {
            throw new IllegalArgumentException("upsert accepts at most " + MAX_BATCH + " records, got " + n);
        }
        String ws = scope(req.getWorkspaceId(), defaultWorkspaceId, "unknown");
        String ds = scope(req.getDataset(), defaultDataset, "default");
        long t0 = System.nanoTime();
        Span span = telemetry.start("upsert", ws, ds, n, null);
        try {
            UpsertResult r = executor.post("/v1/vectors/upsert", payload(req), UpsertResult.class, req.isIdempotent());
            telemetry.success(span, "upsert", ws, r == null ? 0 : r.getIds().size(), t0);
            return r;
        } catch (RuntimeException e) {
            telemetry.error(span, "upsert", ws, e, t0);
            throw e;
        }
    }

    /** Vector search (hybrid by default). Idempotent — auto-retried. */
    public SearchResult search(SearchRequest req) {
        Integer topK = req.getTopK();
        if (topK != null && topK > MAX_TOP_K) {
            throw new IllegalArgumentException("top_k must be <= " + MAX_TOP_K + ", got " + topK);
        }
        Double minScore = req.getMinScore();
        if (minScore != null && (minScore.isNaN() || minScore.isInfinite())) {
            throw new IllegalArgumentException("min_score must be a finite number");
        }
        String ws = scope(req.getWorkspaceId(), defaultWorkspaceId, "unknown");
        String ds = scope(req.getDataset(), defaultDataset, "default");
        SearchMode mode = req.getMode();
        // req_count for search = requested top_k (search has no input batch);
        // -1 when top_k is unset so telemetry omits req_count instead of logging 0.
        int reqTopK = topK == null ? -1 : topK;
        long t0 = System.nanoTime();
        Span span = telemetry.start("search", ws, ds, reqTopK, mode == null ? "hybrid" : mode.wire());
        try {
            SearchResult r = executor.post("/v1/vectors/search", payload(req), SearchResult.class, true);
            telemetry.success(span, "search", ws, r == null ? 0 : r.getHits().size(), t0);
            return r;
        } catch (RuntimeException e) {
            telemetry.error(span, "search", ws, e, t0);
            throw e;
        }
    }

    /** Direct lookup by id. Idempotent — auto-retried. */
    public QueryResult query(QueryRequest req) {
        int n = req.getIds().size();
        validateIds(n, "query");
        String ws = scope(req.getWorkspaceId(), defaultWorkspaceId, "unknown");
        String ds = scope(req.getDataset(), defaultDataset, "default");
        long t0 = System.nanoTime();
        Span span = telemetry.start("query", ws, ds, n, null);
        try {
            QueryResult r = executor.post("/v1/vectors/query", payload(req), QueryResult.class, true);
            telemetry.success(span, "query", ws, r == null ? 0 : r.getHits().size(), t0);
            return r;
        } catch (RuntimeException e) {
            telemetry.error(span, "query", ws, e, t0);
            throw e;
        }
    }

    /** Hard delete by id. Idempotent — auto-retried. */
    public DeleteResult delete(DeleteRequest req) {
        int n = req.getIds().size();
        validateIds(n, "delete");
        String ws = scope(req.getWorkspaceId(), defaultWorkspaceId, "unknown");
        String ds = scope(req.getDataset(), defaultDataset, "default");
        long t0 = System.nanoTime();
        Span span = telemetry.start("delete", ws, ds, n, null);
        try {
            DeleteResult r = executor.post("/v1/vectors/delete", payload(req), DeleteResult.class, true);
            telemetry.success(span, "delete", ws, r == null ? 0 : r.getDeleteCount(), t0);
            return r;
        } catch (RuntimeException e) {
            telemetry.error(span, "delete", ws, e, t0);
            throw e;
        }
    }

    private static void validateIds(int n, String op) {
        if (n == 0) {
            throw new IllegalArgumentException(op + " requires at least one id");
        }
        if (n > MAX_BATCH) {
            throw new IllegalArgumentException(op + " accepts at most " + MAX_BATCH + " ids, got " + n);
        }
    }

    /**
     * Resolves the effective scope value for telemetry labels: per-request
     * override first, then the client default, then a fallback label.
     */
    private static String scope(String reqVal, String defaultVal, String fallback) {
        if (reqVal != null && !reqVal.isEmpty()) {
            return reqVal;
        }
        if (defaultVal != null) {
            return defaultVal;
        }
        return fallback;
    }

    /**
     * Serializes the request and injects the client-level {@code workspace_id} /
     * {@code dataset} defaults when the request did not set them. NON_NULL
     * serialization means unset request fields are absent here, so a missing
     * key is unambiguous.
     */
    private ObjectNode payload(Object request) {
        ObjectNode node = Json.MAPPER.valueToTree(request);
        return applyScopeDefaults(node, defaultWorkspaceId, defaultDataset);
    }

    /**
     * Injects {@code workspace_id} / {@code dataset} defaults into a serialized
     * request body only where the request itself did not set them. Package-private
     * for unit testing. (Returns the same node, mutated.)
     */
    static ObjectNode applyScopeDefaults(ObjectNode node, String defaultWorkspaceId, String defaultDataset) {
        if (!node.hasNonNull("workspace_id") && defaultWorkspaceId != null) {
            node.put("workspace_id", defaultWorkspaceId);
        }
        if (!node.hasNonNull("dataset") && defaultDataset != null) {
            node.put("dataset", defaultDataset);
        }
        return node;
    }

    /** Releases the underlying OkHttp connection pool and dispatcher threads. */
    @Override
    public void close() {
        http.dispatcher().executorService().shutdown();
        http.connectionPool().evictAll();
    }
}
