package csoss.veda.example;

import csoss.veda.sdk.VedaClient;
import csoss.veda.sdk.filter.Filter;
import csoss.veda.sdk.model.DeleteRequest;
import csoss.veda.sdk.model.DeleteResult;
import csoss.veda.sdk.model.QueryRequest;
import csoss.veda.sdk.model.QueryResult;
import csoss.veda.sdk.model.Record;
import csoss.veda.sdk.model.RecordHit;
import csoss.veda.sdk.model.SearchHit;
import csoss.veda.sdk.model.SearchRequest;
import csoss.veda.sdk.model.SearchResult;
import csoss.veda.sdk.model.UpsertRequest;
import csoss.veda.sdk.model.UpsertResult;

/**
 * Pinecone-style usage of Veda's /v1/vectors/* endpoints — the Java twin of
 * examples/python_pinecone_demo.py.
 *
 * Run against a Veda server with a db-kind workspace already created and a wk_
 * workspace key for it (the wk_ binds the workspace). Env vars:
 *
 *   VEDA_URL       e.g. http://localhost:9009
 *   VEDA_API_KEY   wk_... workspace key (data-plane; not an account vk_)
 *
 * First install the SDK locally:  (cd sdk/java && mvn install)
 * Then run:  mvn -f examples/java/pom.xml -q compile exec:java
 */
public final class Demo {

    public static void main(String[] args) {
        String base = requireEnv("VEDA_URL");
        String apiKey = requireEnv("VEDA_API_KEY");

        try (VedaClient veda = VedaClient.builder()
                .baseUrl(base)
                .apiKey(apiKey)
                .build()) {

            // 1. Upsert two records into the bootstrapped "default" dataset.
            UpsertResult upserted = veda.upsert(UpsertRequest.builder()
                    .addRecord(Record.builder().id("sku-1").text("Air Jordan 1").meta("price", 1299).build())
                    .addRecord(Record.builder().id("sku-2").text("Yeezy 350").meta("price", 1599).build())
                    .build());
            System.out.println("upsert ids: " + upserted.getIds());

            // 2. Search with a meta-field filter (price < 1500).
            SearchResult found = veda.search(SearchRequest.builder()
                    .query("sneakers under 1500")
                    .topK(5)
                    .filter(Filter.must().lt("meta.price", 1500))
                    .build());
            for (SearchHit hit : found.getHits()) {
                System.out.printf("  hit: %s score=%.4f (%s) meta=%s%n",
                        hit.getId(), hit.getScore(), hit.getScoreType(), hit.getMeta());
            }

            // 3. Query by id (direct lookup; no ranking).
            QueryResult queried = veda.query(QueryRequest.builder()
                    .ids("sku-1", "sku-2").build());
            StringBuilder ids = new StringBuilder();
            for (RecordHit h : queried.getHits()) {
                ids.append(ids.length() == 0 ? "" : ", ").append(h.getId());
            }
            System.out.println("query hits: [" + ids + "]");

            // 4. Delete both records.
            DeleteResult deleted = veda.delete(DeleteRequest.builder()
                    .ids("sku-1", "sku-2").build());
            System.out.println("delete count: " + deleted.getDeleteCount());
        }
    }

    private static String requireEnv(String name) {
        String v = System.getenv(name);
        if (v == null || v.isEmpty()) {
            System.err.println("missing required env var: " + name);
            System.exit(1);
        }
        return v;
    }

    private Demo() {
    }
}
