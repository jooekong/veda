package csoss.veda.sdk.filter;

import com.fasterxml.jackson.annotation.JsonProperty;

import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collection;
import java.util.Collections;
import java.util.List;
import java.util.regex.Pattern;

/**
 * Fluent builder for the v0 search filter DSL. All clauses are AND-combined
 * ({@code must} only — no {@code should} / {@code must_not}) and the server
 * additionally AND-merges the dataset + active scope.
 *
 * <pre>{@code
 * Filter f = Filter.must()
 *     .lt("meta.price", 1500)
 *     .in("meta.brand", "nike", "adidas");
 * }</pre>
 *
 * <p>This builder enforces the cheap, unambiguous rules locally (field must be
 * {@code meta.<key>} with a single non-nested key; {@code in} must be non-empty
 * and ≤100). Deeper value-type validation is left to the server (single source
 * of truth) and surfaces as {@code INVALID_INPUT}. The full DSL constraints are
 * documented in the README.
 */
public final class Filter {
    /** meta.<key> with a single, non-nested key from the documented charset. */
    private static final Pattern FIELD = Pattern.compile("^meta\\.[a-zA-Z0-9_-]+$");
    private static final int MAX_IN = 100;

    private final List<Clause> must = new ArrayList<>();

    private Filter() {
    }

    /** Starts a new {@code must} (AND) filter. */
    public static Filter must() {
        return new Filter();
    }

    public Filter eq(String field, Object value) {
        return add(field, FilterOp.EQ, requireScalar(field, value));
    }

    public Filter gt(String field, Object value) {
        return add(field, FilterOp.GT, requireScalar(field, value));
    }

    public Filter gte(String field, Object value) {
        return add(field, FilterOp.GTE, requireScalar(field, value));
    }

    public Filter lt(String field, Object value) {
        return add(field, FilterOp.LT, requireScalar(field, value));
    }

    public Filter lte(String field, Object value) {
        return add(field, FilterOp.LTE, requireScalar(field, value));
    }

    public Filter in(String field, Object... values) {
        return in(field, values == null ? null : Arrays.asList(values));
    }

    public Filter in(String field, Collection<?> values) {
        if (values == null || values.isEmpty()) {
            throw new IllegalArgumentException("filter `in` for " + field + " must be a non-empty array");
        }
        if (values.size() > MAX_IN) {
            throw new IllegalArgumentException("filter `in` for " + field + " exceeds " + MAX_IN + " values");
        }
        return add(field, FilterOp.IN, new ArrayList<>(values));
    }

    private Filter add(String field, FilterOp op, Object value) {
        validateField(field);
        must.add(new Clause(field, op, value));
        return this;
    }

    private static Object requireScalar(String field, Object value) {
        if (value == null) {
            throw new IllegalArgumentException("filter value for " + field + " must not be null");
        }
        return value;
    }

    private static void validateField(String field) {
        if (field == null || !FIELD.matcher(field).matches()) {
            throw new IllegalArgumentException(
                    "filter field must be `meta.<key>` with a single non-nested key matching [a-zA-Z0-9_-]+, got: " + field);
        }
    }

    /** True when no clauses were added (the SDK omits an empty filter). */
    public boolean isEmpty() {
        return must.isEmpty();
    }

    @JsonProperty("must")
    public List<Clause> getMust() {
        return Collections.unmodifiableList(must);
    }

    /** One {@code {field, op, value}} clause. */
    public static final class Clause {
        private final String field;
        private final FilterOp op;
        private final Object value;

        Clause(String field, FilterOp op, Object value) {
            this.field = field;
            this.op = op;
            this.value = value;
        }

        @JsonProperty("field")
        public String getField() {
            return field;
        }

        @JsonProperty("op")
        public FilterOp getOp() {
            return op;
        }

        @JsonProperty("value")
        public Object getValue() {
            return value;
        }
    }
}
