package csoss.veda.sdk.internal;

import com.csoss.monitor.api.trace.Span;
import csoss.veda.sdk.error.ErrorCode;
import csoss.veda.sdk.error.VedaApiException;
import org.junit.jupiter.api.Test;

import static org.junit.jupiter.api.Assertions.assertDoesNotThrow;
import static org.junit.jupiter.api.Assertions.assertNull;

/**
 * Telemetry must be a safe no-op in two cases: when disabled, and when the host
 * never initialized monitoring (the unit-test case — {@code Metrics}/{@code
 * Traces} then return no-op instruments). It must never throw into the
 * data-plane call path.
 */
class TelemetryTest {

    @Test
    void disabledCreatesNoSpanAndNeverThrows() {
        Telemetry t = new Telemetry(false);
        Span span = t.start("upsert", "ws-1", "products", 3, null);
        assertNull(span, "disabled telemetry must not open a span");
        long t0 = System.nanoTime();
        assertDoesNotThrow(() -> t.success(span, "upsert", "ws-1", 3, t0));
        assertDoesNotThrow(() -> t.error(span, "upsert", "ws-1",
                new VedaApiException(ErrorCode.INTERNAL, 500, "boom"), t0));
    }

    @Test
    void enabledWithoutInitRunsFullPathWithoutThrowing() {
        Telemetry t = new Telemetry(true);
        long t0 = System.nanoTime();
        // success path: span + op counter + latency + rows counter
        assertDoesNotThrow(() -> {
            Span span = t.start("search", "ws-1", "products", 10, "hybrid");
            t.success(span, "search", "ws-1", 5, t0);
        });
        // error path: span exception + error-status counters
        assertDoesNotThrow(() -> {
            Span span = t.start("delete", "ws-1", "products", 2, null);
            t.error(span, "delete", "ws-1",
                    new VedaApiException(ErrorCode.UNAUTHORIZED, 401, "bad token"), t0);
        });
    }

    @Test
    void failsClosedOnHostileInputWithoutThrowing() {
        // Even nulls that could NPE inside the monitor stack must not escape —
        // instrumentation is fail-closed so it can never break the call path.
        Telemetry t = new Telemetry(true);
        long t0 = System.nanoTime();
        assertDoesNotThrow(() -> {
            Span span = t.start(null, null, null, -1, null);
            t.success(span, null, null, 0, t0);
        });
        assertDoesNotThrow(() -> t.error(null, null, null,
                new VedaApiException(ErrorCode.INTERNAL, 500, "x"), t0));
    }
}
