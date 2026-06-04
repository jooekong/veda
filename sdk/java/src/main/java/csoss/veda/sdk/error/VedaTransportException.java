package csoss.veda.sdk.error;

/**
 * Thrown when the request never produced a usable error envelope: connection
 * failure, timeout, or a response body that was not valid JSON. Wraps the
 * underlying {@link java.io.IOException} (or parse error) as the cause.
 */
public class VedaTransportException extends VedaException {
    private static final long serialVersionUID = 1L;

    public VedaTransportException(String message, Throwable cause) {
        super(message, cause);
    }
}
