package csoss.veda.sdk;

import csoss.veda.sdk.internal.HttpExecutor;
import csoss.veda.sdk.internal.Telemetry;
import okhttp3.OkHttpClient;

import java.util.concurrent.TimeUnit;

/**
 * Configures and builds a {@link VedaClient}.
 *
 * <p>{@code baseUrl} and {@code apiKey} are required. {@code workspaceId} is
 * strongly recommended: when omitted, the server only resolves it implicitly if
 * the token is scoped to exactly one workspace, otherwise it returns
 * {@code INVALID_INPUT}. {@code dataset} is optional (the server falls back to
 * {@code default}). Both can be overridden per request.
 */
public final class VedaClientBuilder {
    private String baseUrl;
    private String apiKey;
    private String workspaceId;
    private String dataset;
    private long connectTimeoutMs = 10_000L;
    private long requestTimeoutMs = 60_000L;
    private int maxRetries = 2;
    private boolean observability = true;

    VedaClientBuilder() {
    }

    /**
     * Server base URL. Required. Production is
     * {@code https://veda.ddmc-inc.com}, test is
     * {@code https://veda.dbpaas.dingdongxiaoqu.com}; a self-hosted server
     * listens on {@code http://localhost:3000} by default.
     */
    public VedaClientBuilder baseUrl(String baseUrl) {
        this.baseUrl = baseUrl;
        return this;
    }

    /** Account-level {@code vk_} token (or scoped service token). Required. */
    public VedaClientBuilder apiKey(String apiKey) {
        this.apiKey = apiKey;
        return this;
    }

    /** Default workspace id; recommended. Overridable per request. */
    public VedaClientBuilder workspaceId(String workspaceId) {
        this.workspaceId = workspaceId;
        return this;
    }

    /** Default dataset; omit to use the server's {@code default}. Overridable per request. */
    public VedaClientBuilder dataset(String dataset) {
        this.dataset = dataset;
        return this;
    }

    /** TCP connect timeout (default 10s). */
    public VedaClientBuilder connectTimeoutMs(long connectTimeoutMs) {
        this.connectTimeoutMs = connectTimeoutMs;
        return this;
    }

    /** Per-request read/write timeout (default 60s). */
    public VedaClientBuilder requestTimeoutMs(long requestTimeoutMs) {
        this.requestTimeoutMs = requestTimeoutMs;
        return this;
    }

    /** Max automatic retries for idempotent calls (default 2; 0 disables). */
    public VedaClientBuilder maxRetries(int maxRetries) {
        if (maxRetries < 0) {
            throw new IllegalArgumentException("maxRetries must be >= 0");
        }
        this.maxRetries = maxRetries;
        return this;
    }

    /**
     * Emit per-op trace spans and metrics via the company monitor stack
     * (default {@code true}). The SDK never initializes monitoring itself; when
     * the host app hasn't, emission is a safe no-op. Set {@code false} to skip
     * instrumentation entirely.
     */
    public VedaClientBuilder observability(boolean enabled) {
        this.observability = enabled;
        return this;
    }

    public VedaClient build() {
        if (baseUrl == null || baseUrl.trim().isEmpty()) {
            throw new IllegalArgumentException("baseUrl is required");
        }
        if (apiKey == null || apiKey.trim().isEmpty()) {
            throw new IllegalArgumentException("apiKey is required");
        }
        String normalizedBase = baseUrl.trim();
        while (normalizedBase.endsWith("/")) {
            normalizedBase = normalizedBase.substring(0, normalizedBase.length() - 1);
        }

        OkHttpClient http = new OkHttpClient.Builder()
                .connectTimeout(connectTimeoutMs, TimeUnit.MILLISECONDS)
                .readTimeout(requestTimeoutMs, TimeUnit.MILLISECONDS)
                .writeTimeout(requestTimeoutMs, TimeUnit.MILLISECONDS)
                // The SDK owns all retry logic to honor the upsert idempotency
                // boundary; don't let OkHttp silently re-send on its own.
                .retryOnConnectionFailure(false)
                .build();

        HttpExecutor executor = new HttpExecutor(http, normalizedBase, apiKey.trim(), maxRetries);
        return new VedaClient(http, executor, new Telemetry(observability),
                emptyToNull(workspaceId), emptyToNull(dataset));
    }

    private static String emptyToNull(String s) {
        return (s == null || s.trim().isEmpty()) ? null : s.trim();
    }
}
