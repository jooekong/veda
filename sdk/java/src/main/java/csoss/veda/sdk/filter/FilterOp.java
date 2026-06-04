package csoss.veda.sdk.filter;

import com.fasterxml.jackson.annotation.JsonValue;

import java.util.Locale;

/** Comparison operators of the v0 filter DSL. */
public enum FilterOp {
    EQ,
    IN,
    GT,
    GTE,
    LT,
    LTE;

    @JsonValue
    public String wire() {
        return name().toLowerCase(Locale.ROOT);
    }
}
