package csoss.veda.sdk.error;

/**
 * Base type for everything the SDK throws. Unchecked so callers are not forced
 * to wrap every call, but can catch {@link VedaApiException} /
 * {@link VedaTransportException} when they want to branch on the cause.
 */
public class VedaException extends RuntimeException {
    private static final long serialVersionUID = 1L;

    public VedaException(String message) {
        super(message);
    }

    public VedaException(String message, Throwable cause) {
        super(message, cause);
    }
}
