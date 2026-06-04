package csoss.veda.sdk.error;

/**
 * Thrown when the server returned a well-formed error envelope
 * ({@code {"success":false,"error_code":...,"error":...}}). Carries the
 * machine-readable {@link ErrorCode} and the HTTP status so callers can branch
 * precisely without parsing the human-readable message.
 */
public class VedaApiException extends VedaException {
    private static final long serialVersionUID = 1L;

    private final ErrorCode errorCode;
    private final int httpStatus;

    public VedaApiException(ErrorCode errorCode, int httpStatus, String message) {
        super(message);
        this.errorCode = errorCode;
        this.httpStatus = httpStatus;
    }

    /** Stable error code; {@link ErrorCode#UNKNOWN} for codes this SDK does not know. */
    public ErrorCode getErrorCode() {
        return errorCode;
    }

    /** HTTP status code that accompanied the error envelope. */
    public int getHttpStatus() {
        return httpStatus;
    }

    /**
     * Whether retrying the same request may succeed. True for transient
     * server-side conditions: the {@link ErrorCode} is retryable, or the HTTP
     * status is 429 or 5xx. The transport layer only auto-retries when this is
     * true <em>and</em> the operation is idempotent.
     */
    public boolean isRetryable() {
        return errorCode.isRetryable() || httpStatus == 429 || httpStatus >= 500;
    }

    @Override
    public String getMessage() {
        return "[" + errorCode + " / HTTP " + httpStatus + "] " + super.getMessage();
    }
}
