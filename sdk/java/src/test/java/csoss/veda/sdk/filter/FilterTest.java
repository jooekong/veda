package csoss.veda.sdk.filter;

import com.fasterxml.jackson.databind.JsonNode;
import csoss.veda.sdk.internal.Json;
import org.junit.jupiter.api.Test;

import java.util.Collections;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

class FilterTest {

    @Test
    void serializesToMustClauses() throws Exception {
        Filter f = Filter.must()
                .lt("meta.price", 1500)
                .in("meta.brand", "nike", "adidas");

        JsonNode node = Json.MAPPER.valueToTree(f);
        JsonNode must = node.get("must");
        assertEquals(2, must.size());

        assertEquals("meta.price", must.get(0).get("field").asText());
        assertEquals("lt", must.get(0).get("op").asText());
        assertEquals(1500, must.get(0).get("value").asInt());

        assertEquals("meta.brand", must.get(1).get("field").asText());
        assertEquals("in", must.get(1).get("op").asText());
        assertTrue(must.get(1).get("value").isArray());
        assertEquals("nike", must.get(1).get("value").get(0).asText());
    }

    @Test
    void rejectsFieldWithoutMetaPrefix() {
        assertThrows(IllegalArgumentException.class, () -> Filter.must().eq("price", 10));
    }

    @Test
    void rejectsNestedMetaKey() {
        assertThrows(IllegalArgumentException.class, () -> Filter.must().eq("meta.a.b", 1));
    }

    @Test
    void rejectsEmptyIn() {
        assertThrows(IllegalArgumentException.class,
                () -> Filter.must().in("meta.brand", Collections.emptyList()));
    }

    @Test
    void rejectsOversizedIn() {
        Integer[] values = new Integer[101];
        for (int i = 0; i < values.length; i++) {
            values[i] = i;
        }
        assertThrows(IllegalArgumentException.class, () -> Filter.must().in("meta.n", (Object[]) values));
    }

    @Test
    void rejectsNullScalarValue() {
        assertThrows(IllegalArgumentException.class, () -> Filter.must().eq("meta.x", null));
    }

    @Test
    void emptyFilterIsEmpty() {
        assertTrue(Filter.must().isEmpty());
    }
}
