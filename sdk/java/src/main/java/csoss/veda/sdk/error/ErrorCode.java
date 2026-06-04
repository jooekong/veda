package csoss.veda.sdk.error;

/**
 * Stable, machine-readable error codes returned by the Veda API in the
 * {@code error_code} field of a failure envelope. Clients match on this
 * enum, never on the human-readable {@code error} text (which may change).
 *
 * <p>Any code the server sends that this SDK does not recognise maps to
 * {@link #UNKNOWN} rather than throwing — this is the forward-compatibility
 * contract, so a newer server adding an error code never breaks an older SDK.
 */
public enum ErrorCode {
    /** 400 — generic validation failure (charset / length / missing field). */
    INVALID_INPUT,
    /** 400 — vector API hit an fs workspace (or vice versa). */
    WORKSPACE_KIND_MISMATCH,
    /** 400 — attempted to delete the implicit {@code default} dataset. */
    CANNOT_DELETE_DEFAULT_DATASET,
    /** 401 — missing / invalid / expired bearer token. */
    UNAUTHORIZED,
    /** 403 — authenticated, but token scope does not cover the target. */
    PERMISSION_DENIED,
    /** 404 — workspace / dataset / token not found or archived. */
    NOT_FOUND,
    /** 409 — name conflict (dataset / email). */
    ALREADY_EXISTS,
    /** 413 — batch size limit exceeded (records/ids &gt;500, top_k &gt;100). */
    PAYLOAD_TOO_LARGE,
    /** 429 — rate limited (reserved; vector API does not emit this yet). */
    QUOTA_EXCEEDED,
    /** 500 — upstream embedding failure. */
    EMBEDDING_FAILED,
    /** 500 — catch-all backend failure (details intentionally withheld). */
    INTERNAL,
    /** Fallback for any code not known to this SDK version. */
    UNKNOWN;

    /**
     * Maps a wire {@code error_code} string to an enum constant, returning
     * {@link #UNKNOWN} for unrecognised or null values.
     */
    public static ErrorCode fromWire(String code) {
        if (code == null) {
            return UNKNOWN;
        }
        for (ErrorCode c : values()) {
            if (c != UNKNOWN && c.name().equals(code)) {
                return c;
            }
        }
        return UNKNOWN;
    }

    /**
     * Whether an operation that failed with this code is worth retrying on its
     * own merits (transient server-side conditions). Note {@code UNKNOWN} is
     * treated as non-retryable here; the transport layer still applies HTTP
     * status heuristics (5xx / 429) on top of this.
     */
    public boolean isRetryable() {
        switch (this) {
            case QUOTA_EXCEEDED:
            case EMBEDDING_FAILED:
            case INTERNAL:
                return true;
            default:
                return false;
        }
    }
}
