package csoss.veda.sdk.error;

import org.junit.jupiter.api.Test;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertTrue;

class ErrorCodeTest {

    @Test
    void knownCodeMaps() {
        assertEquals(ErrorCode.INVALID_INPUT, ErrorCode.fromWire("INVALID_INPUT"));
        assertEquals(ErrorCode.PAYLOAD_TOO_LARGE, ErrorCode.fromWire("PAYLOAD_TOO_LARGE"));
    }

    @Test
    void unknownCodeFallsBackToUnknown() {
        assertEquals(ErrorCode.UNKNOWN, ErrorCode.fromWire("SOME_FUTURE_CODE"));
        assertEquals(ErrorCode.UNKNOWN, ErrorCode.fromWire(null));
    }

    @Test
    void retryableClassification() {
        assertTrue(ErrorCode.INTERNAL.isRetryable());
        assertTrue(ErrorCode.EMBEDDING_FAILED.isRetryable());
        assertTrue(ErrorCode.QUOTA_EXCEEDED.isRetryable());
        assertFalse(ErrorCode.INVALID_INPUT.isRetryable());
        assertFalse(ErrorCode.UNAUTHORIZED.isRetryable());
        assertFalse(ErrorCode.UNKNOWN.isRetryable());
    }

    @Test
    void apiExceptionRetryableByStatus() {
        assertTrue(new VedaApiException(ErrorCode.INTERNAL, 500, "x").isRetryable());
        assertTrue(new VedaApiException(ErrorCode.QUOTA_EXCEEDED, 429, "x").isRetryable());
        // Unknown code but 5xx status → still retryable on transport grounds.
        assertTrue(new VedaApiException(ErrorCode.UNKNOWN, 503, "x").isRetryable());
        assertFalse(new VedaApiException(ErrorCode.INVALID_INPUT, 400, "x").isRetryable());
        assertFalse(new VedaApiException(ErrorCode.UNAUTHORIZED, 401, "x").isRetryable());
    }
}
