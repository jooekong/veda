package csoss.veda.sdk.internal;

import com.fasterxml.jackson.core.JsonParser;
import com.fasterxml.jackson.databind.DeserializationContext;
import com.fasterxml.jackson.databind.JsonDeserializer;

import java.io.IOException;
import java.time.Instant;

/**
 * Deserializes a wire int64 epoch-millis value into an {@link Instant}. The
 * data-plane uses epoch-ms (not RFC3339) for vector-hit {@code created_at} /
 * {@code updated_at}; this keeps a single dependency-free time type on the
 * model. Null / missing stays null.
 */
public final class EpochMilliInstantDeserializer extends JsonDeserializer<Instant> {
    @Override
    public Instant deserialize(JsonParser p, DeserializationContext ctxt) throws IOException {
        if (p.getCurrentToken() != null && p.getCurrentToken().isNumeric()) {
            return Instant.ofEpochMilli(p.getLongValue());
        }
        String raw = p.getValueAsString();
        if (raw == null || raw.isEmpty()) {
            return null;
        }
        return Instant.ofEpochMilli(Long.parseLong(raw.trim()));
    }
}
