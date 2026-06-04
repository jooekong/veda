package csoss.veda.sdk.internal;

import com.csoss.monitor.api.common.Attributes;
import com.csoss.monitor.api.metrics.Metrics;
import com.csoss.monitor.api.trace.Span;
import com.csoss.monitor.api.trace.StatusCode;
import com.csoss.monitor.api.trace.Traces;
import csoss.veda.sdk.error.VedaApiException;

/**
 * Optional trace + metric instrumentation for the four data-plane operations,
 * bridged to the company monitor stack ({@code com.csoss.monitor}, OTLP gRPC).
 *
 * <p>Each op opens a span {@code veda.db.<op>} carrying workspace / dataset /
 * request &amp; result row counts, and records three metrics:
 * <ul>
 *   <li>{@code veda_db_op_total} — op count, labels {op, workspace, status}</li>
 *   <li>{@code veda_db_op_latency_ms} — latency histogram, same labels</li>
 *   <li>{@code veda_db_rows_total} — rows returned, labels {op, workspace}</li>
 * </ul>
 *
 * <p>The SDK never calls {@code MonitorInitializer}; it only emits. When the host
 * application has not initialized monitoring, {@code Metrics}/{@code Traces}
 * return no-op instruments (no NPE). Disable emission entirely via
 * {@code VedaClient.builder().observability(false)}.
 *
 * <p>Metric names are a fixed low-cardinality set (the stack caps the number of
 * distinct names and silently drops the overflow), so high-cardinality values
 * such as {@code dataset} live only on the span, never in a metric name.
 */
public final class Telemetry {
    private static final String SPAN_PREFIX = "veda.db.";
    private static final String M_OP = "veda_db_op_total";
    private static final String M_LATENCY = "veda_db_op_latency_ms";
    private static final String M_ROWS = "veda_db_rows_total";

    private final boolean enabled;

    public Telemetry(boolean enabled) {
        this.enabled = enabled;
    }

    /**
     * Opens a span for one operation. Returns {@code null} when disabled or if
     * starting the span itself fails; pass the returned value back to
     * {@link #success} / {@link #error}. Never throws.
     *
     * @param reqCount input row count; a negative value omits the attribute
     * @param mode     the search ranker name, or {@code null} for non-search ops
     */
    public Span start(String op, String workspace, String dataset, int reqCount, String mode) {
        if (!enabled) {
            return null;
        }
        try {
            Span span = Traces.spanBuilder(SPAN_PREFIX + op).startSpan();
            span.setAttribute("op", op);
            span.setAttribute("workspace", workspace);
            span.setAttribute("dataset", dataset);
            if (reqCount >= 0) {
                span.setAttribute("req_count", (long) reqCount);
            }
            if (mode != null) {
                span.setAttribute("mode", mode);
            }
            return span;
        } catch (Throwable t) {
            // Instrumentation must never break the call path.
            return null;
        }
    }

    /**
     * Records a successful op: result row count + OK on the span, counters +
     * latency. Never throws — a telemetry failure must not turn a successful call
     * into a failed one. Always ends the span.
     */
    public void success(Span span, String op, String workspace, int resultCount, long startNano) {
        if (!enabled) {
            return;
        }
        try {
            long elapsedMs = (System.nanoTime() - startNano) / 1_000_000L;
            if (span != null) {
                span.setAttribute("result_count", (long) resultCount);
                span.setStatus(StatusCode.OK);
            }
            emitMetrics(op, workspace, "ok", elapsedMs);
            Metrics.newCounter(M_ROWS).build()
                    .add(resultCount, Attributes.builder().put("op", op).put("workspace", workspace).build());
        } catch (Throwable t) {
            // swallow — never fail the business call because of instrumentation
        } finally {
            endQuietly(span);
        }
    }

    /**
     * Records a failed op: exception + ERROR on the span, error counters +
     * latency. Never throws — it must not replace the original business
     * exception that the caller is about to rethrow. Always ends the span.
     */
    public void error(Span span, String op, String workspace, RuntimeException e, long startNano) {
        if (!enabled) {
            return;
        }
        try {
            long elapsedMs = (System.nanoTime() - startNano) / 1_000_000L;
            String errorCode = (e instanceof VedaApiException)
                    ? ((VedaApiException) e).getErrorCode().name()
                    : "TRANSPORT";
            if (span != null) {
                span.setAttribute("error_code", errorCode);
                span.recordException(e);
                span.setStatus(StatusCode.ERROR);
            }
            emitMetrics(op, workspace, "error", elapsedMs);
        } catch (Throwable t) {
            // swallow — must not mask the caller's original exception
        } finally {
            endQuietly(span);
        }
    }

    private void emitMetrics(String op, String workspace, String status, long elapsedMs) {
        Attributes labels = Attributes.builder()
                .put("op", op).put("workspace", workspace).put("status", status).build();
        Metrics.newCounter(M_OP).build().add(1, labels);
        Metrics.newTimer(M_LATENCY).build().value(elapsedMs, labels);
    }

    private static void endQuietly(Span span) {
        if (span == null) {
            return;
        }
        try {
            span.end();
        } catch (Throwable t) {
            // ignore — end() is idempotent and must not throw into the caller
        }
    }
}
