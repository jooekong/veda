# 方案：Veda db workspace 的 Java SDK（仅数据面）

> 状态：**已发布 `csoss.veda:veda-sdk-java:0.0.1-SNAPSHOT` 到 ddxq Nexus（2026-06-04）**。4 端点 + filter/error/retry + **monitor 埋点** + 单测(28) + 集成/e2e 测试 + README + demo 全部落地于 `sdk/java`；坐标/Nexus（原 D-b）已定，CI **不**加 sdk 发布 job（Joe 2026-06-04）。见 §9/§12/§14。
> 历史：2026-06-02 实现含 sparse/hybrid/min_score、e2e 2026-06-03 对线上实跑全绿（§8）；v2 过 Codex review（§13），D-a~D-g 全收口。
> 决策基准（2026-06-02 与 Joe 敲定）：**手写精简 SDK / Java 8 基线 / monorepo `sdk/java` + 内部 Maven / 仅数据面 4 端点**。
> 权威 API 参考：`docs/api/db-workspace-api.md`（专为写 SDK 准备）。覆盖 `docs/plans/db-workspace-followups.md` D1（与其原倾向 generator 相反，理由见 §1）。
> **归档注记（2026-06-10）**：§4/§5/D-g 的 auth 示例（`.apiKey("vk_...")` + workspaceId 必配）是 `fa7f91c`（2026-06-03）之前的旧契约——现行数据面为 `wk_`、body 不带 workspace_id，SDK 的 wk_ 适配 + write_mode 支持未做（见 todos.md）。现行契约以 `sdk/java/README.md` + `docs/api/db-workspace-api.md` 为准。

---

## 1. 目标与范围

给 db workspace 的 Pinecone 式 REST/JSON 数据面做一层地道的 Java 封装。**不动 server**——server 契约已就位（强类型 DTO、统一 `ApiResponse<T>` 信封、稳定 `error_code`）。

**做（仅数据面 4 端点）**：

| 方法 | 端点 | 语义 |
|---|---|---|
| `upsert` | `POST /v1/vectors/upsert` | 按 `(dataset,id)` 插入/整行替换，≤500/次 |
| `search` | `POST /v1/vectors/search` | `mode`=hybrid(默认)/semantic/fulltext + filter DSL + `min_score`（仅 semantic/fulltext） |
| `query` | `POST /v1/vectors/query` | 按 id 直查，≤500/次 |
| `delete` | `POST /v1/vectors/delete` | 按 id 硬删，≤500/次 |

**+ observability**（2026-06-04 追加）：4 个操作自动埋 trace span + metric（含 workspace、返回行数），见 §14。

**不做**（本期）：控制面（workspace/dataset/admin-token）、账号面（注册/登录）、fs 能力、OpenAPI generator、gRPC、Spring Boot Starter。假设业务方已用控制台/curl 预置好 workspace + `vk_` token。

**为什么手写而非 generator**（推翻 D1 原倾向）：数据面就 4 端点、~12 个 DTO；generator 生成不了真正有价值的部分（`error_code`→异常、filter builder、幂等感知重试、未知字段前向兼容），那些反正得手写包一层；而手写避免了维护 OpenAPI spec（漂移）或给 axum 全量加 `utoipa` 注解（侵入）。契约漂移用 §8 的真实 server 一致性测试兜底。

---

## 2. 技术选型与依赖

| 项 | 选择 | 理由 |
|---|---|---|
| 语言级别 | Java 8 | Joe 定的基线；覆盖老 Spring Boot。无 records → POJO + builder。 |
| HTTP | **OkHttp**（D-a 定） | Joe 选 OkHttp：超时/重试/连接池成熟、API 体验好。代价：拖 kotlin-stdlib + okio 传递依赖（业务方多已有 OkHttp，冲突风险可控）。 |
| JSON | Jackson `jackson-databind` | Spring Boot 生态默认，业务方几乎都有；只需一个 artifact。 |
| 构建 | Maven | 业务方主流；对齐内部 Nexus 发布。 |

**最终依赖面**：`jackson-databind` + `okhttp`（传递拖 kotlin-stdlib + okio）+ **`com.csoss:monitor-instrumentation`**（observability 硬依赖，传递拖 grpc/protobuf/snappy，见 §14）。**不引** Lombok（避免业务方加注解处理器）。注意：OkHttp 默认仅重试连接失败；**5xx / 幂等感知重试（§7）由 SDK 自管一层 interceptor**，不全靠 OkHttp 默认行为。

---

## 3. 目录与坐标

```
sdk/java/                      # monorepo 新增顶层
  pom.xml                      # groupId/artifactId 见 D-b
  src/main/java/<pkg>/
    VedaClient.java            # 入口：配置 + 4 方法
    VedaClientBuilder.java
    model/                     # DTO：Record / SearchHit / RecordHit / *Request / *Result
    filter/Filter.java         # fluent filter builder
    error/                     # ErrorCode enum + 异常层级
    internal/                  # HTTP 执行、JSON、重试（不导出）
  src/test/java/<pkg>/         # 契约一致性测试（连真实 server）
  README.md
examples/java/                 # 对齐 examples/python_pinecone_demo.py 的 Java demo
```

`sdk/java` 是独立 maven 项目（**不**进 Cargo workspace），CI 单独 job 构建（§9）。

---

## 4. 公共 API 形态（Java 8 builder 风格）

```java
VedaClient veda = VedaClient.builder()
    .baseUrl("http://10.79.51.161:9009")
    .apiKey("vk_...")
    .workspaceId("ws-...")               // 推荐必配；省略仅 token scoped 到 1 个 ws 时 server 才隐式解析，否则 INVALID_INPUT（见 §5/D-g）
    .dataset("products")                 // 可选默认，省略=server 的 "default"
    .connectTimeoutMs(10_000)
    .requestTimeoutMs(60_000)
    .maxRetries(2)                       // 见 §7 幂等边界
    .build();

// upsert —— 怕重试就自带 id（省略 id 非幂等，SDK 会拒绝自动重试，见 §7）
UpsertResult up = veda.upsert(UpsertRequest.builder()
    .addRecord(Record.builder()
        .id("sku-1").text("Air Jordan 1")
        .category("shoes").tags("sale", "new")
        .meta(Collections.singletonMap("price", 1299))
        .build())
    .build());
up.getIds();        // 写入 id（已对同批重复 id 去重，可能短于请求）
up.getCommitTs();   // epoch ms

// search
SearchResult s = veda.search(SearchRequest.builder()
    .query("sneakers under 1500")
    .mode(SearchMode.SEMANTIC)           // 可选：HYBRID(默认)/SEMANTIC/FULLTEXT
    .topK(10)
    .minScore(0.4)                       // 可选相关度下限；仅 SEMANTIC/FULLTEXT，配 HYBRID 会被 server 400
    .filter(Filter.must()
        .lt("meta.price", 1500)
        .eq("meta.category", "shoes"))   // field 必须 meta.<key>；builder 校验前缀
    .outputFields("text", "meta")        // 投影白名单，可选
    .build());
for (SearchHit h : s.getHits()) {
    h.getId(); h.getScore();             // 越大越相关，含义看 score_type
    h.getScoreType();                    // "cosine"/"bm25"/"rrf"，跨 mode 不可比
    h.getMeta();                         // 宽容 Object；object meta 用 h.metaAsMap()
}

// query（也支持 output_fields 投影，与 search 同语义）/ delete
QueryResult q = veda.query(QueryRequest.builder()
    .ids("sku-1", "sku-2").outputFields("text", "meta").build());
DeleteResult d = veda.delete(DeleteRequest.builder().ids("sku-1", "sku-2").build());
d.getDeleteCount();   // = len(ids)，是 tombstone 数，非实删数（javadoc 注明，引导先 query）
```

设计取向（贴合简洁偏好）：一个 `VedaClient` 入口；请求用 builder（Java 8 无 records）；`workspaceId`/`dataset` 在 client 设默认、每请求可覆盖；**不**为每个端点造 service 子对象（4 个方法直接挂 client）。

---

## 5. DTO ↔ wire 映射

| Java 类型 | wire 字段 | 注意 |
|---|---|---|
| `Record.id` | `id?` | 省略→server 生成 UUID（insert-only，**非幂等**）。SDK javadoc 强引导自带 id。 |
| `Record.text/category/tags/meta` | 同名 | 写入 `meta` = `Map<String,Object>`（D-f，有意收窄到 object）；`tags` = `List<String>`。 |
| `SearchHit` | id/dataset/category/tags/text/meta/created_at/updated_at/**score/score_type** | `created_at`/`updated_at` = int64 epoch ms → `Instant`（`EpochMilliInstantDeserializer`）。投影字段被 `output_fields` 排除时为 null。`score` = `double`。 |
| `SearchHit.meta`（读） | `meta` | 宽容 `Object`（object→Map、array→List、scalar→boxed），`metaAsMap()` 便捷取 object（D-f 读取宽容，防历史非 object 值炸）。 |
| `SearchHit.scoreType` | `score_type` ✅ | `"cosine"`(语义~[0,1]) / `"bm25"`(全文~[0,30+]) / `"rrf"`(hybrid~[0,0.033])，**跨 mode 不可比**；缺省 `"cosine"`（兼容 pre-mode 旧 payload）。 |
| `RecordHit` | 同 SearchHit **但无 score/score_type** | query 命中项（按 id 直查，非排序）。 |
| `SearchRequest.mode` | `mode` ✅ | `SearchMode`：`HYBRID`(**默认**)/`SEMANTIC`/`FULLTEXT`，`@JsonValue`→snake_case。不传 → server 走 hybrid。 |
| `SearchRequest.minScore` | `min_score` ✅ | `Double` 相关度下限。**仅 `semantic`/`fulltext`**；配 `hybrid`（含默认）→ server `400`。在 `top_k` 之后裁剪，结果可能少于 `top_k`。 |

**前向兼容关键**：所有响应 DTO 加 `@JsonIgnoreProperties(ignoreUnknown = true)`（+ 全局 `FAIL_ON_UNKNOWN_PROPERTIES=false`）。`score_type` 落地正是靠这条平滑接住——新 SDK 读旧 payload 靠 `default` 回退 `"cosine"`，旧 SDK 读新 payload 不炸。下一个 additive 字段同样无痛。

**`workspaceId`/`dataset` 省略语义**（codex [15]）：`dataset` 省略 → server 取 `default`（安全）。`workspaceId` 省略 → server **仅在 token 恰好 scoped 到 1 个 ws 时**隐式解析，否则返 `INVALID_INPUT`。SDK 不在 client 拦截（不知 token scope），但**推荐 client 必配 `workspaceId`**，javadoc 写清该规则。

---

## 6. 错误处理

解析 `ApiResponse`：`success=false` → 按 `error_code` 抛异常。**只认 `error_code`，不解析 `error` 文案**。

```
VedaException (RuntimeException 基类)
├── VedaApiException        // server 返回了错误信封
│     ErrorCode getErrorCode();  int getHttpStatus();  String getMessage();  boolean isRetryable();
└── VedaTransportException  // 连不上/超时/响应非法 JSON（IOException 包装）
```

`ErrorCode` enum 覆盖文档全集 + `UNKNOWN` 兜底（**未知码不抛 ClassNotFound 类错误，归 UNKNOWN**，前向兼容）：
`INVALID_INPUT / WORKSPACE_KIND_MISMATCH / CANNOT_DELETE_DEFAULT_DATASET / UNAUTHORIZED / PERMISSION_DENIED / NOT_FOUND / ALREADY_EXISTS / PAYLOAD_TOO_LARGE / QUOTA_EXCEEDED / EMBEDDING_FAILED / INTERNAL / UNKNOWN`。

不为每个码建一个异常类（过度抽象）——一个 `VedaApiException` 带 enum 字段 + `isRetryable()` 即可。

---

## 7. 横切：超时 / 重试 / auth / 本地预校验

- **auth**：每请求注入 `Authorization: Bearer <apiKey>` + `Content-Type: application/json`。
- **超时**：connect/request 两段超时可配（默认 10s/60s，对齐现有 CLI/FUSE client）。
- **重试（幂等边界，SDK 的核心价值）**：
  - 可重试：网络错误、HTTP `5xx`（含 `EMBEDDING_FAILED`/`INTERNAL`）、`429 QUOTA_EXCEEDED`。指数退避（对齐 server 内部退避风格）。
  - **不重试** `4xx`（除 429）——必失败，重试无意义。
  - **upsert 特例**：仅当**所有 record 都自带 `id`**（幂等）才参与自动重试；**存在省略 id 的 record（非幂等）→ 该 upsert 禁用自动重试**，避免网络重试导致重复写。这是文档反复强调的坑，SDK 替业务方守住。
- **filter builder 引导**（codex [7]）：提供类型安全 op 方法（`eq/in/gt/gte/lt/lte`）+ 强制 `field` 以 `meta.` 前缀开头；**完整 DSL 约束**（仅 `must`、no nested、key 字符集 `[a-zA-Z0-9_-]+`、range/in 的 value 类型、`in` 非空 ≤100）写进 README/javadoc。**深度校验仍交 server**（不在 client 重写规则，避免双写漂移）。
- **本地预校验（仅最低成本项，省一次必失败往返）**：`records`/`ids` 非空且 ≤500、`top_k` ≤100、`min_score` 有限值（非 NaN/Inf）、filter `in` 数组 ≤100。**字段长度/字符集、`min_score`×`hybrid` 互斥不在 client 拦**（server 权威；互斥交 server 返 `INVALID_INPUT`，IT 已验证）→ 避免规则双写漂移。
- **search mode 契约**（调用方须知）：不传 `mode` → **server 默认 `hybrid`**，`score` 是 RRF（`score_type=rrf`），与 cosine/bm25 量纲不可比。**`hybrid` 后端失败 server 直接 5xx、不静默降级 semantic**——SDK 如实抛 `VedaApiException`（幂等会按本节重试 5xx），调用方不会拿到"偷偷换成 semantic"的结果。

---

## 8. 测试策略（真实 server，禁 mock）

对齐项目既有约定（整合测试用真实服务、CI 不连内网）：**不 mock server**，连真实 veda + Milvus + embedding。它是**防 Rust 契约漂移的发版 gate**——server DTO 改了它会挂；但因 CI 连不上内网真实服务（与 server 集成测试同约定），**它不在默认 CI 跑，发 SDK tag 前必须手动跑通**（触发责任写进发布 checklist，见 §9）。

两套真实 server 测试（`@EnabledIfEnvironmentVariable` gate，env 缺失自动 skip，都不进默认 CI）：

**`VedaClientIT`**（gate=`VEDA_URL`，需预置 `VEDA_WS_ID`，测数据面契约）：
- **全链路** `fullLifecycle`：upsert（同批重复 id → 去重成 2 ids）→ search+filter（命中 + score 降序 + `created_at`→`Instant` + **默认 hybrid → `rrf`**）→ query（`output_fields` 投影）→ delete（`delete_count == len(ids)`）。
- **sparse 三 mode** `searchModesReportScoreType`：`SEMANTIC→cosine` / `FULLTEXT→bm25` / `HYBRID→rrf`。
- **min_score** `minScoreFiltersSemanticAndRejectsHybrid`：semantic 高 floor 过滤；**hybrid+min_score → `400`**。
- **错误映射**：坏 token→`UNAUTHORIZED`、range op 传 bool→`INVALID_INPUT`（codex [13]）。

**`VedaE2EIT`**（gate=`VEDA_BASE_URL`，**自包含**，对标 server `remote_e2e_test.rs`）：裸 HTTP bootstrap 账号+db ws（控制面在 SDK 外）→ 全程走 SDK → best-effort 删 ws；**poll 抗 Milvus 可见性延迟**；用 server e2e 同款**正交文档证伪**——fulltext 稀有词只命中所属文档（断言**不含**另一篇）、hybrid RRF top1 + dense leg 改写召回、min_score 中点 floor（model-independent）。**补上了原残留缺口①的 fulltext 证伪。**

**实跑状态（2026-06-03）**：`VEDA_BASE_URL=https://veda.dbpaas.dingdongxiaoqu.com mvn -o -f sdk/java/pom.xml -P integration verify` → **`VedaE2EIT` 4 个全绿 + 25 单测全过**（连真实 Milvus/embedding）。⚠️ 首跑曾 3 失败，根因=线上 alpha 当时仍是旧版（sparse 未部署 → `mode`/`min_score` 被忽略、hits 无 `score_type`），裸 curl 坐实后部署新版重跑全绿——**"已实现"≠"已上线"，发版前必实跑**。
- **跑法**：`mvn -f sdk/java/pom.xml verify -P integration`（failsafe 跑 `*IT`）；默认 `mvn test` 只跑纯逻辑单测（surefire 排除 `*IT`）。
- **残留缺口（backlog，非阻塞）**：前向兼容靠 `ignoreUnknown` 被动保证，无主动"注入未知字段"单测。

---

## 9. 发布

- **坐标（已定）**：`csoss.veda:veda-sdk-java`（对齐公司 redis sdk 的 `csoss.*` group）。Java 包名同步为 `csoss.veda.sdk`。
- **版本**：SDK 独立版本，**不强绑 server 版本号**。**首发 `0.0.1-SNAPSHOT`（2026-06-04）**。SDK 内暴露 `VedaClient.SUPPORTED_API = "v0"` 常量。
- **内部 Maven（已通）**：`pom.xml` `<distributionManagement>` 指向 ddxq Nexus（release=`maven-releases` / snapshot=`maven-snapshots`，repo id 对齐 `~/.m2/settings.xml` 的 `ddmc-repo`/`snapshots` server 认证）；`<repositories>` 加 `maven-public` 解析 `com.csoss:*`。`mvn -f sdk/java/pom.xml clean deploy` → `0.0.1-SNAPSHOT` 已上传 `maven-snapshots`。
- **CI**：**不加 sdk 发布 job**（Joe 2026-06-04）；发布手动 `mvn deploy`。

---

## 10. 文档与示例

- `sdk/java/README.md`：5 分钟接入（拿 token → new client → upsert/search）、幂等与重试语义、错误码表、依赖说明。
- `examples/java/`：对齐 `examples/python_pinecone_demo.py` 的 Java demo（同样四步 + 同样 env 变量），让业务方一眼对照。
- 在 `docs/api/db-workspace-api.md` 顶部加一行指向本 SDK。

---

## 11. 决策点（待 Codex / Joe）

- **D-a HTTP client** → **已定：OkHttp**（Joe 2026-06-02）。超时/重试/连接池成熟；接受 kotlin-stdlib+okio 传递依赖。
- **D-b 坐标与 Nexus** → **已定（Joe 2026-06-04）**：`csoss.veda:veda-sdk-java`（包名同步 `csoss.veda.sdk`），首发 `0.0.1-SNAPSHOT`，发 ddxq Nexus；已实际 `mvn clean deploy` 成功（§9/§14）。
- **D-c 时间字段** → **采用推荐：`Instant`**（内部按 epoch ms 反序列化）。
- **D-d score_type** → **已落地**：`SearchHit.scoreType` 实装，server 已返三值（cosine/bm25/rrf）。
- **D-e 重试默认** → **已落地**：`maxRetries` 默认 2；省略 id 的 upsert `maxAttempts=1`（`HttpExecutor`，非幂等只发一次）。
- **D-f meta 类型** → **已定（Joe 2026-06-02）：写入 `Record.meta = Map<String,Object>`**（有意收窄到 object，javadoc 说明 v0 非 object 无法被 filter）+ **读取 `hit.meta` 用宽容类型（`Object`/`JsonNode`）** 防历史非 object 值反序列化炸。
- **D-g 默认作用域** → **采用推荐：client 必配 `workspaceId`** + 每请求可覆盖（server 省略规则有歧义，见 §5）；`dataset` 省略安全（取 `default`）。

---

## 12. 工作量与 DoD

**工作量**（手写、仅数据面）：脚手架(maven+CI+坐标) ~0.5d；DTO+client+filter+异常 ~1d；重试/超时/auth/JSON 横切 ~0.5d；真实 server 一致性测试调通 ~1d；README+Java demo+发布流水线 ~0.5d。**合计 ~3.5d**。

**DoD**（实现 2026-06-02；e2e 2026-06-03 对线上 `dbpaas` 部署实跑全绿）：
- [x] 4 端点 + sparse 三 mode + min_score 对真实 server 跑通：**`VedaE2EIT` 2026-06-03 对线上实跑 4 个全绿**（自包含 bootstrap + 正交文档证伪 + poll 抗可见性）；`VedaClientIT`（需预置 ws）作数据面契约补充。无 env 自动 skip。
- [x] search 与 query 的 `output_fields` 投影：SDK 支持两端 `outputFields`，e2e/IT 断言并实跑确认。
- [x] `error_code` 全映射 + 未知码归 `UNKNOWN`；响应 `@JsonIgnoreProperties(ignoreUnknown)` + 全局 `FAIL_ON_UNKNOWN_PROPERTIES=false` 前向兼容（含 `score_type` 缺省→cosine）。**单测覆盖、绿。**
- [x] upsert 幂等边界：id-less record 存在则该批 `idempotent=false`、executor 单发不重试；OkHttp `retryOnConnectionFailure(false)` 交 SDK 独管；幂等 upsert/search/query/delete 退避重试 5xx/429/网络错。幂等检测 + 预校验**单测覆盖、绿**（重试时序行为靠结构保证，未起 mock server）。
- [x] 依赖最小（仅 `jackson-databind` + `okhttp`），无 Lombok。
- [x] README + `examples/java` demo：done（README 5 分钟接入 + 错误/重试/filter/mode/observability 全表；demo 对齐 python 版四步）。
- [x] **内部 Maven 发布（done 2026-06-04）**：坐标 `csoss.veda:veda-sdk-java:0.0.1-SNAPSHOT`，`mvn clean deploy` 已上传 ddxq `maven-snapshots`。CI 不加 sdk 发布 job（Joe 定）。
- [x] **monitor 埋点（done 2026-06-04，§14）**：4 op 各 span `veda.db.<op>` + metric（op_total/op_latency_ms/rows_total，workspace 进 label）；硬依赖 monitor-instrumentation；SDK 只 emit 不 init；fail-closed 不污染业务调用；过 codex review。

> 验证：`mvn test` **28 单测全绿**（含 monitor `TelemetryTest` 3）；**`VEDA_BASE_URL=… mvn -P integration verify` → `VedaE2EIT` 4 个对线上实跑全绿**（2026-06-03，真实 Milvus/embedding）；`mvn clean deploy` → `0.0.1-SNAPSHOT` 已上传 ddxq Nexus。
> **无开放项。** 后续按需迭代版本。

---

## 13. Codex review 处置（2026-06-02，xhigh effort）

Codex 实读 5 个源文件，18 条 finding：**11 keep**（时间格式 / upsert 幂等 / 错误码全集 / 前向兼容 / delete_count / 范围与抽象 / Java8+HttpURLConnection+Jackson 选型 / 重试幂等边界 / 429-5xx-4xx 分类 / 决策点 D-a~D-e / mode-score_type 预留 —— 逐条确认与 `api.rs`/`vectors.rs` 实际行为一致）+ **6 fix** + **1 minor**。6 条 fix 的独立处置：

| # | codex finding | 处置 |
|---|---|---|
| 5 | query 也支持 `output_fields` | **采纳**：`QueryRequest` 加 `outputFields`（§4/§5/DoD）。 |
| 7 | filter DSL 约束不完整 | **部分采纳**：builder 做前缀+op 引导、README 列全约束（§7）；深度校验仍交 server（守不双写原则，拒绝 client 重写全部规则=over-engineering）。 |
| 12 | 集成测试非 CI 门禁，不能称"自动兜底" | **采纳措辞**：改为"发版 gate，tag 前手动跑、责任入发布 checklist"（§8/§9），对齐 server 集成测试约定（CI 连不上内网）。 |
| 13 | filter 边界测试用例表述有误 | **采纳**：改为非法 filter value / nested / platform field（§8）。 |
| 14 | `meta` 收窄为 object 与 wire 任意 JSON 不符 | **升级为决策**：D-f 给"写入收窄 Map + 读取宽容"方案，待 Joe 拍板。 |
| 15 | `workspaceId` 省略语义含糊 | **采纳**：§5 写清"仅 token 单 ws 才隐式解析"，SDK 推荐必配（§4/D-g）。 |
| 18 | 缺 sparse 契约测试计划 | **已落地**：server 落地 sparse/hybrid/min_score（commit `1f6ac0b`），`VedaClientIT` 三 mode + score_type 三值 + min_score 互斥全覆盖（§8）。残留 fulltext 证伪用例记 backlog。 |

Codex 总评（v2）："不能 as-is 进入实现，需先修订（6 阻塞项）"。6 条已全部落地。

**v3 增量（2026-06-02，commit `1f6ac0b feat(vectors)!`）**：server 落地 db sparse/hybrid/`min_score`，SDK 同步实装 `mode`/`score_type`/`min_score`，由 `VedaClientIT` 三 mode + score_type 三值 + min_score 互斥覆盖。本文档由"待实现方案"追平为"已实现契约参考"。契约要点（调用方须知）：**默认 mode=hybrid → rrf 分数**、**hybrid 失败不降级直接报错**、**min_score 仅 semantic/fulltext**、**score 跨 mode 不可比（先看 score_type）**。

---

## 14. monitor 埋点 + 发布（2026-06-04）

Joe 追加需求：给 4 个数据面操作加 trace/metric，含 workspace 信息与返回行数。参考公司 redis sdk（`csoss.redis`）的 monitor 用法。

**设计决策（Joe 2026-06-04 拍板）**：
- **硬依赖** `com.csoss:monitor-instrumentation:1.1.5-RELEASE`（业务方多为 Spring、已有 monitor）。注意 `monitor-agent` 只是 Spring 自动装配壳，真正 API 在传递依赖 `monitor-common` 的 `com.csoss.monitor.api.*`（`Metrics`/`Traces`/`Attributes`/`Span`）。传递拖 grpc-netty/protobuf/snappy ≈ 几 MB，README 已注明足迹。
- **workspace 进 metric label**（按 ws 聚合）；**dataset 只进 span**（防 series 基数爆——monitor 单 metric ~200 series 上限、超限静默丢、name 强制小写）。
- 保留薄 `Telemetry` 类（去重 + `observability` 开关 + 可测），**不抄**公司 redis sdk 的 iface+impl+Dummy+Factory 重封装（v0 不必要，monitor 本身关闭即 noop）。
- SDK **只 emit、绝不 `MonitorInitializer`**；host 未 init 时靠 monitor 自带 noop（实测不抛）。OTLP 默认发本地 agent 5317(trace)/5318(metric)。

**实装**：
- `internal/Telemetry.java`：span `veda.db.<op>`（attr: workspace/dataset/op/req_count/result_count/mode/error_code）+ counter `veda_db_op_total`{op,workspace,status} + timer `veda_db_op_latency_ms` + counter `veda_db_rows_total`{op,workspace}。
- `VedaClient` 4 方法织入（解析生效 ws/ds → span → 成功记返回行数/失败记 error_code）；`VedaClientBuilder.observability(boolean)` 开关（默认开）。

**codex review（2026-06-04，xhigh）**：1 Critical + 5 Minor，逐条 filter 后全处置：
- **Critical（埋点异常污染业务）→ 修**：`Telemetry` **fail-closed** —— start/success/error 各 try-catch(Throwable) 吞埋点自身异常、绝不外抛；error 不覆盖原始业务异常；`endQuietly` 在 finally 兜底 `span.end()`。新增 `failsClosedOnHostileInput` 单测。
- search `req_count` 在 topK=null 时记 0 → 改传 -1（不记）；README 删 span `status` 行 + 补 cardinality budget 警告 + 补依赖足迹 + 注明"独立 span"。
- makeCurrent 不改（v0 SDK 内无嵌套子 span）。
- 注：首轮 codex 卡在重复 grounding（反复 `jar tf` 私有 monitor jar）30min 被 cancel；重发时把 API 速查塞进 prompt + 禁 grounding，5min 出结果。教训：给 codex review 私有依赖时，预先喂 API 速查、明令禁 grounding。

**发布**：`mvn -f sdk/java/pom.xml clean deploy` → `csoss.veda:veda-sdk-java:0.0.1-SNAPSHOT` 上传 ddxq `maven-snapshots`（28 单测全绿）。业务方坐标 `csoss.veda:veda-sdk-java:0.0.1-SNAPSHOT`。
