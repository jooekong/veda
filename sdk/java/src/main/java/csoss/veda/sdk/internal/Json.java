package csoss.veda.sdk.internal;

import com.fasterxml.jackson.annotation.JsonInclude;
import com.fasterxml.jackson.databind.DeserializationFeature;
import com.fasterxml.jackson.databind.ObjectMapper;

/**
 * Single configured {@link ObjectMapper} for the SDK.
 *
 * <ul>
 *   <li>{@code NON_NULL} serialization: request DTOs omit unset fields so the
 *       server applies its documented defaults (e.g. omitted {@code id} → UUID,
 *       omitted {@code dataset} → "default").</li>
 *   <li>{@code FAIL_ON_UNKNOWN_PROPERTIES=false}: forward compatibility — a
 *       newer server adding response fields never breaks deserialization (this
 *       backstops the per-class {@code @JsonIgnoreProperties} annotations).</li>
 * </ul>
 */
public final class Json {
    public static final ObjectMapper MAPPER = new ObjectMapper()
            .setSerializationInclusion(JsonInclude.Include.NON_NULL)
            .configure(DeserializationFeature.FAIL_ON_UNKNOWN_PROPERTIES, false);

    private Json() {
    }
}
