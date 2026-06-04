package csoss.veda.sdk;

import com.fasterxml.jackson.databind.node.ObjectNode;
import csoss.veda.sdk.internal.Json;
import csoss.veda.sdk.model.DeleteRequest;
import csoss.veda.sdk.model.QueryRequest;
import csoss.veda.sdk.model.Record;
import csoss.veda.sdk.model.SearchRequest;
import csoss.veda.sdk.model.UpsertRequest;
import org.junit.jupiter.api.Test;

import java.util.ArrayList;
import java.util.List;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

/**
 * Unit tests for client-side pre-validation and request shaping — no network.
 * A client is built with a dummy base URL; every assertion fails (throws) before
 * any HTTP call is attempted.
 */
class VedaClientTest {

    private VedaClient dummyClient() {
        return VedaClient.builder().baseUrl("http://localhost:1").apiKey("vk_test").build();
    }

    @Test
    void builderRequiresBaseUrlAndApiKey() {
        assertThrows(IllegalArgumentException.class, () -> VedaClient.builder().apiKey("vk_x").build());
        assertThrows(IllegalArgumentException.class, () -> VedaClient.builder().baseUrl("http://x").build());
        assertThrows(IllegalArgumentException.class, () -> VedaClient.builder().maxRetries(-1));
    }

    @Test
    void upsertRejectsEmptyAndOversizedBatch() {
        try (VedaClient c = dummyClient()) {
            assertThrows(IllegalArgumentException.class, () -> c.upsert(UpsertRequest.builder().build()));

            UpsertRequest.Builder b = UpsertRequest.builder();
            for (int i = 0; i < 501; i++) {
                b.addRecord(Record.builder().id("id-" + i).text("t").build());
            }
            assertThrows(IllegalArgumentException.class, () -> c.upsert(b.build()));
        }
    }

    @Test
    void queryAndDeleteRejectEmptyAndOversizedIds() {
        try (VedaClient c = dummyClient()) {
            assertThrows(IllegalArgumentException.class, () -> c.query(QueryRequest.builder().build()));
            assertThrows(IllegalArgumentException.class, () -> c.delete(DeleteRequest.builder().build()));

            List<String> ids = new ArrayList<>();
            for (int i = 0; i < 501; i++) {
                ids.add("id-" + i);
            }
            assertThrows(IllegalArgumentException.class, () -> c.delete(DeleteRequest.builder().ids(ids).build()));
        }
    }

    @Test
    void searchRejectsBadTopKAndMinScore() {
        try (VedaClient c = dummyClient()) {
            assertThrows(IllegalArgumentException.class,
                    () -> c.search(SearchRequest.builder().query("x").topK(101).build()));
            assertThrows(IllegalArgumentException.class,
                    () -> c.search(SearchRequest.builder().query("x").minScore(Double.NaN).build()));
            assertThrows(IllegalArgumentException.class,
                    () -> c.search(SearchRequest.builder().query("x").minScore(Double.POSITIVE_INFINITY).build()));
        }
    }

    @Test
    void upsertIdempotencyDetection() {
        UpsertRequest withIds = UpsertRequest.builder()
                .addRecord(Record.builder().id("a").text("t").build())
                .addRecord(Record.builder().id("b").text("t").build())
                .build();
        assertTrue(withIds.isIdempotent());

        UpsertRequest mixed = UpsertRequest.builder()
                .addRecord(Record.builder().id("a").text("t").build())
                .addRecord(Record.builder().text("no-id").build())
                .build();
        assertFalse(mixed.isIdempotent());
    }

    @Test
    void scopeDefaultsInjectedOnlyWhenAbsent() {
        ObjectNode node = Json.MAPPER.createObjectNode();
        VedaClient.applyScopeDefaults(node, "ws-default", "ds-default");
        assertEquals("ws-default", node.get("workspace_id").asText());
        assertEquals("ds-default", node.get("dataset").asText());

        ObjectNode override = Json.MAPPER.createObjectNode();
        override.put("workspace_id", "ws-req");
        VedaClient.applyScopeDefaults(override, "ws-default", "ds-default");
        assertEquals("ws-req", override.get("workspace_id").asText(), "per-request override must win");
        assertEquals("ds-default", override.get("dataset").asText());
    }
}
