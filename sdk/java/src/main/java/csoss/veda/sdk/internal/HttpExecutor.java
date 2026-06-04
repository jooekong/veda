package csoss.veda.sdk.internal;

import com.fasterxml.jackson.databind.JsonNode;
import csoss.veda.sdk.error.ErrorCode;
import csoss.veda.sdk.error.VedaApiException;
import csoss.veda.sdk.error.VedaTransportException;
import okhttp3.MediaType;
import okhttp3.OkHttpClient;
import okhttp3.Request;
import okhttp3.RequestBody;
import okhttp3.Response;
import okhttp3.ResponseBody;

import java.io.IOException;
import java.util.concurrent.ThreadLocalRandom;

/**
 * Sends one JSON request to the Veda API and decodes the {@code ApiResponse}
 * envelope, with idempotency-aware retry.
 *
 * <p>Retry policy: network failures and retryable API errors (HTTP 429 / 5xx,
 * or {@code EMBEDDING_FAILED} / {@code INTERNAL} / {@code QUOTA_EXCEEDED}) are
 * retried with exponential backoff <em>only</em> when the call is idempotent.
 * Non-idempotent calls (an upsert batch with any id-less record) get exactly
 * one attempt so a network retry can never duplicate a write. OkHttp's own
 * connection-failure retry is disabled so this layer is the sole authority.
 */
public final class HttpExecutor {
    private static final MediaType JSON = MediaType.parse("application/json; charset=utf-8");
    private static final long BACKOFF_BASE_MS = 200L;
    private static final long BACKOFF_CAP_MS = 5_000L;

    private final OkHttpClient http;
    private final String baseUrl;
    private final String apiKey;
    private final int maxRetries;

    public HttpExecutor(OkHttpClient http, String baseUrl, String apiKey, int maxRetries) {
        this.http = http;
        this.baseUrl = baseUrl;
        this.apiKey = apiKey;
        this.maxRetries = maxRetries;
    }

    /**
     * POSTs {@code payload} (already scope-resolved JSON) to {@code path} and
     * maps the envelope's {@code data} to {@code type}.
     *
     * @throws VedaApiException       on a server error envelope
     * @throws VedaTransportException on network / timeout / malformed-JSON failure
     */
    public <T> T post(String path, JsonNode payload, Class<T> type, boolean idempotent) {
        String url = baseUrl + path;
        byte[] body;
        try {
            body = Json.MAPPER.writeValueAsBytes(payload);
        } catch (IOException e) {
            throw new VedaTransportException("failed to serialize request body for " + path, e);
        }
        Request request = new Request.Builder()
                .url(url)
                .header("Authorization", "Bearer " + apiKey)
                .post(RequestBody.create(body, JSON))
                .build();

        int maxAttempts = idempotent ? (maxRetries + 1) : 1;
        for (int attempt = 1; attempt <= maxAttempts; attempt++) {
            boolean last = attempt == maxAttempts;

            int code;
            String bodyStr;
            try (Response resp = http.newCall(request).execute()) {
                code = resp.code();
                ResponseBody rb = resp.body();
                bodyStr = rb == null ? "" : rb.string();
            } catch (IOException io) {
                if (last) {
                    throw new VedaTransportException("transport error calling " + url, io);
                }
                backoff(attempt);
                continue;
            }

            JsonNode root;
            try {
                root = Json.MAPPER.readTree(bodyStr);
            } catch (IOException parse) {
                if (code >= 500 && !last) {
                    backoff(attempt);
                    continue;
                }
                throw new VedaTransportException("non-JSON response (HTTP " + code + ") from " + url, parse);
            }

            if (root == null || root.isMissingNode() || root.isNull()) {
                if (code >= 200 && code < 300) {
                    return null;
                }
                if (code >= 500 && !last) {
                    backoff(attempt);
                    continue;
                }
                throw new VedaTransportException("empty response (HTTP " + code + ") from " + url, null);
            }

            if (root.path("success").asBoolean(false)) {
                JsonNode data = root.get("data");
                if (data == null || data.isNull()) {
                    return null;
                }
                try {
                    return Json.MAPPER.convertValue(data, type);
                } catch (IllegalArgumentException convEx) {
                    throw new VedaTransportException("failed to map response data from " + url, convEx);
                }
            }

            ErrorCode ec = ErrorCode.fromWire(root.path("error_code").asText(null));
            String msg = root.path("error").asText("");
            VedaApiException apiEx = new VedaApiException(ec, code, msg);
            if (apiEx.isRetryable() && !last) {
                backoff(attempt);
                continue;
            }
            throw apiEx;
        }
        throw new IllegalStateException("retry loop exited without a result");
    }

    private static void backoff(int attempt) {
        long base = Math.min(BACKOFF_CAP_MS, BACKOFF_BASE_MS * (1L << (attempt - 1)));
        long jitter = ThreadLocalRandom.current().nextLong(base / 2 + 1);
        try {
            Thread.sleep(base + jitter);
        } catch (InterruptedException ie) {
            Thread.currentThread().interrupt();
            throw new VedaTransportException("interrupted during retry backoff", ie);
        }
    }
}
