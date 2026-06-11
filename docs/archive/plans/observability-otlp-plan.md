# 方案：veda 接入公司可观测（一期 Metrics，OTLP 桥接）

> 状态：✅ 一期（metrics）已实现并上线（`62de928`，2026-06-05；两 box 灰度 + 平台查询验证完成）。2026-06-10 归档。
> **trace 二期开工前先读本文 §0 协议事实**——datapoint label 查询、非标 ID、env.yaml 权限坑等对 trace 同样适用（todos.md 有指针）。
> 协议事实见 memory `reference_company_observability` / `cs-oss/monitor-agent` 文档；不重复抄，本文聚焦 veda 侧落地。
> 关联：veda 现有 metrics 栈（`obs.rs` + `metrics` 0.24 + `metrics-exporter-prometheus` 0.16）。

---

## 0. 协议事实速查（接手必读，自包含）

公司监控 = **OpenTelemetry OTLP，仅 gRPC + protobuf**。权威文档在 GitLab `git.ddxq.mobi/cs-oss/monitor-agent` 的 `docs/`（`README.md`、`OTHER_LANGUAGE_SDK_INTEGRATION.md`、`PYTHON_SDK_INTEGRATION.md`、`AI_METRICS_SPEC.md`、`proto/csoss-monitor-otlp-entrypoint.proto`）。下面是落地必需的事实（已实测）：

- **调用**：官方 `opentelemetry.proto.collector.metrics.v1.MetricsService/Export`（`ExportMetricsServiceRequest`）。**旧 proto 结构** `ResourceMetrics → InstrumentationLibraryMetrics`（**不是**新版 `ScopeMetrics`），对齐 `opentelemetry-proto 1.4.1-alpha`。
- **Collector 地址 = 配置服务下发**：`monitor`（从 `/etc/ddmc/env.yaml`）拼 `https://{monitor}/api/v1/configs/{appname}/version/1/agent` → 返回 `{"collectors":{"metrics":["10.79.11.x:5318",...],"traces":[...:5317]}}`（全局列表、`version` 被忽略、HA 多实例、客户端轮询 + 失败重连）。本地兜底默认 metrics `127.0.0.1:5318` / trace `5317`。
  - 实测拉配置：`ssh root@10.79.51.161 "curl -sk https://paasconf-hw-sh.ddmc-inc.com/api/v1/configs/dbpaas-ai-service/version/1/agent"`
  - 连通验证：`nc -z` 会**假阴性**（被网络策略挡）；以 `curl --http2-prior-knowledge http://10.79.11.108:5318/` 得 `http2 + 415`（gRPC 活着）或实际 `Export` 返回 `OK` 为准。
- **Resource 必填**：`appname`/`env_name`/`ip`/`sdk_version`（建议加 `host`/`env_level`/`zone`/`sdk.language=rust`/`service.name`/`deployment.environment`）。`appname`/`env_name`/`env_level`/`zone` 从 `/etc/ddmc/env.yaml`（`.161` 上 `appname=dbpaas-ai-service`、`env_name=hw-test`）。
- **三个非标准坑**（正是不用官方 opentelemetry-rust 的原因）：① metrics DataPoint **双写** `attributes`(KeyValue) + deprecated `labels`(StringKeyValue)，当前 Collector 是旧 receiver、读 `labels`；② proto 用 `InstrumentationLibrary`（非 `Scope`）；③ trace（二期）ID 是**变长字符串** UTF-8 bytes（非 16/8 随机 bytes）。
- **别踩**：`.161` 上 `monitor-agent:7890` 是 ddmc 的 **cAdvisor**（容器指标，HTTP/Prometheus），**不是** OTLP 口。
- **机器接入**：`root@10.79.51.161`（dogfood，veda 跑 `/data/veda/bin/veda-server`，已建 ssh ControlMaster）。`env.yaml`/`iaas.yaml` 在 `/etc/ddmc/`。

---

## 1. 目标与范围

把 veda 现有的 Prometheus 指标，**额外**以 OTLP gRPC 推送到公司 Collector，让 veda 进公司监控平台（按 `appname=dbpaas-ai-service` 查）。

- **做**：metrics 的 Prometheus→OTLP 桥接（周期推送）。
- **不做（本期）**：trace（二期手搓）、AI `gen_ai_*` 指标、metrics↔trace 反查（`metrics_mapping`）。
- **不动**：现有 `/v1/metrics` Prometheus pull 端点保留（双轨，OTLP 是新增的 push）。

---

## 2. 关键技术决策：**不用 opentelemetry-rust**

公司 Collector 有三个非标准要求，官方 `opentelemetry-otlp`/`opentelemetry-sdk` 都满足不了，**确认不用**：

| 公司要求 | opentelemetry-rust 现状 | 结论 |
|---|---|---|
| `InstrumentationLibraryMetrics`（旧 proto） | 用 `ScopeMetrics`（新） | ✗ |
| `NumberDataPoint` 双写 `attributes`+deprecated `labels` | 新 proto 已无 `labels` 字段 | ✗ |
| （trace）变长字符串 ID | `TraceId` 固定 16 bytes | ✗（二期 trace 才涉及） |

→ 自己用 **`tonic` + `prost`** 引入旧版 OTLP proto（含 `InstrumentationLibrary` + `labels`），手写一个轻量 exporter。veda 当前**零 gRPC 依赖**（Milvus 走 REST），是从零加 gRPC 栈，但范围可控（只 1 个 unary 调用 `MetricsService/Export`）。

---

## 3. 方案设计

### 3.1 数据来源 —— 复用现有 Prometheus，解析 text

`MetricsHandle` 只暴露 `render() -> String`（Prometheus text exposition format），没有结构化快照 API。最简路径是**解析这段 text**：

- 新增 crate `prometheus-parse`，把 `state.metrics.render()` 解析成结构化 `Scrape`（含 `# TYPE` 标注的 counter/gauge/histogram + labels + 值）。
- 类型从 Prometheus 的 `# TYPE` 注释直接拿（不用猜），histogram 的 `_bucket{le}`/`_sum`/`_count` 由 parser 归组。

> 备选（v2，若 text 解析成瓶颈/精度不够）：用 `metrics-util` 的 `FanoutBuilder`，在 `obs::install()` 里给全局 recorder 叠一个自定义 snapshot recorder，直接拿结构化数据。**MVP 不上**——解析 text 改动最小、不碰 recorder 装配。

### 3.2 OTLP proto 生成

- 从 `cs-oss/monitor-agent` 仓库 `docs/proto/` + 它列的官方 proto（**含 `InstrumentationLibrary`+`labels` 的旧版**；⚠️ otel-proto **v0.19 删了 `InstrumentationLibrary`、v0.20 删了 `labels`**，别误拿新版）拿 `.proto`，**vendored 进 `crates/veda-server/proto/` 并在文件头记录来源 cs-oss commit SHA**（pin 死，防后续误更新到 `ScopeMetrics`/无 `labels`，Codex 边缘 #3）。
- 加 `build.rs` 用 `tonic-build` 生成 Rust（`ExportMetricsServiceRequest`、`ResourceMetrics`、`InstrumentationLibraryMetrics`、`Metric`、`NumberDataPoint`/`HistogramDataPoint`、`KeyValue`、`StringKeyValue` 等）。
- 只需 `MetricsService` 一个 stub（trace service 二期再加）。

### 3.3 指标映射（含 labels 双写）

| Prometheus 类型 | OTLP 类型 | 要点 |
|---|---|---|
| counter | `Sum`（`is_monotonic=true`, `CUMULATIVE`） | 值 `as_double`（见下） |
| gauge | `Gauge` | 值 `as_double` |
| histogram | `Histogram`（`CUMULATIVE`） | bucket 要**差分**，见下 |

每个 DataPoint 的维度**同时写** `attributes`（`KeyValue`）**和** deprecated `labels`（`StringKeyValue`，value 一律 `toString`）——这是当前 Collector 能查到数据的前提（见 reference memory：Collector 是"旧 receiver"，读 `labels`）。

**⚠️ histogram 累积桶 → 独立桶（必做差分，Codex #1）**：Prometheus `_bucket{le}` 是**累积**计数（`le` 含所有 ≤le 样本），OTLP `HistogramDataPoint.bucket_counts` 是**每桶独立**计数。转换：有限 `le` 升序排、相邻差分得每桶 count；`explicit_bounds` = 有限 `le` 列表（**不含 `+Inf`**）；`+Inf` 桶 = `count − 最后一个有限累积桶`，作为 `bucket_counts` 的**最后一个**元素。满足 `bucket_counts.len == explicit_bounds.len + 1`。不差分会 `sum(bucket_counts) > count`、分布全错。

**时间语义（cumulative，Codex #2）**：exporter **启动时记一个固定 `start`**（epoch nanos），所有 cumulative datapoint（Sum/Histogram）写 `start_time_unix_nano = start`、`time_unix_nano = now`；Gauge 只写 `time_unix_nano`。进程重启 `start` 重置（Collector 按 cumulative 重启处理）。

**值类型（Codex #3）**：数据源是 Prometheus text，解析后 Rust 原始类型已丢、值统一 float 语法。**MVP 一律 `as_double`**（含 counter/gauge），避免"按实际类型"不可执行；后续若监控端要整型，再加 `fract()==0` 才 `as_int` 的判断 + 测试。

- veda 现有**十余个**指标（HTTP 请求/延迟、embedding、LLM、outbox、retention、drift、mysql pool 等，分布在 `obs.rs`/`main.rs`/`worker.rs`/`reconciler.rs`/`veda-pipeline`），全量桥接；DoD 的 golden test 要覆盖**实际全集**（非示例 9 个）。
- `InstrumentationLibrary.name` 用 `"monitor"`（文档建议的内部 SDK 指标库名）。

### 3.4 Resource attributes

必填 `appname`/`env_name`/`ip`/`sdk_version`，建议 `host`/`env_level`/`zone`/`sdk.language`(=`rust`)/`service.name`(=appname)/`deployment.environment`(=env_name)。来源：

- `appname`/`env_name`/`env_level`/`zone` ← 读 `/etc/ddmc/env.yaml`（用 `serde_yaml`）。
- `ip` ← 本机非 loopback IP（运行时获取）；`host` ← hostname。
- `sdk_version` ← veda 版本（`env!("CARGO_PKG_VERSION")`，前缀 `rust-`）。
- 兜底：env.yaml 缺失时从 config / 环境变量取，否则禁用 OTLP 并 warn（不 panic）。

### 3.5 Collector 发现与发送

- 启动时 HTTP GET（复用 `reqwest`）拉配置：`https://{monitor}/api/v1/configs/{appname}/version/1/agent`（`monitor` 来自 env.yaml），取 `collectors.metrics`（12 个 `10.79.11.x:5318`）。
- gRPC（tonic）unary 调 `MetricsService/Export`，**轮询**列表、失败（`UNAVAILABLE`/`UNIMPLEMENTED`/`RESOURCE_EXHAUSTED`）换下一个并定期刷新列表；10s deadline；`monitor.compression` 暂不开。
- 直连覆盖：config 允许写死 endpoint（本地有 agent 时发 `127.0.0.1:5318`，或测试指定）。

### 3.6 后台任务 + config

- 照 `reconciler` 模式：`main.rs` 里 `tokio::spawn` 一个 `OtlpExporter::run(shutdown_rx)`，`tokio::select!` + `interval(5s)`，每周期 render→parse→map→export，复用现有 `shutdown_rx` 优雅退出。
- 新增 config 段（仿 `ReconcilerConfig`，serde default + `VEDA_OTLP_*` env 覆盖）：

```toml
[otlp]
enabled       = false   # 默认关，灰度开
interval_secs = 5
endpoint      = ""      # 空=走配置服务发现；非空=直连覆盖（如本地 agent 127.0.0.1:5318）
env_yaml_path = "/etc/ddmc/env.yaml"  # resource + monitor(配置服务) 来源
appname       = ""      # 空=从 env.yaml 读；非空=覆盖
env_name      = ""      # 同上
monitor       = ""      # 配置服务 host；空=读 env.yaml 的 monitor
```
env 覆盖（仿 `VEDA_RECONCILER_*`）：`VEDA_OTLP_ENABLED` / `_INTERVAL_SECS` / `_ENDPOINT` / `_ENV_YAML_PATH` / `_APPNAME` / `_ENV_NAME` / `_MONITOR`。`ip`(本机非 loopback)、`host`(hostname)、`sdk_version`(`rust-{CARGO_PKG_VERSION}`) 运行时取，不入 config。env.yaml 不可读且 config 未覆盖 `appname`/`monitor` → 禁用 OTLP + warn（Codex #4）。

---

## 4. 落地点

```
crates/veda-server/
  proto/                       # 新：OTLP .proto（旧版，含 InstrumentationLibrary+labels）
  build.rs                     # 新：tonic-build 生成
  src/
    obs.rs → obs/mod.rs        # 现有 metrics frontend（基本不动）
    obs/otlp/
      mod.rs                   # OtlpExporter：run loop + 发送
      proto.rs                 # tonic 生成代码 include
      convert.rs               # prometheus Scrape → OTLP request（映射 + labels 双写）
      resource.rs              # env.yaml + 本机 ip/host/version → Resource
      discovery.rs             # 拉配置服务取 collector 列表 + 轮询
    config.rs                  # 新增 OtlpConfig 段
    main.rs                    # spawn exporter 任务
```

**新增依赖**（veda-server）：`tonic`、`prost`、`tonic-build`(build-dep)、`prometheus-parse`、`serde_yaml`；`reqwest`（workspace 有，但 veda-server 当前只在 **dev-dependencies**，discovery 放 server 需**提为正式依赖**，Codex 边缘 #1）。`tokio` 已有。**不引** opentelemetry。

---

## 5. 分步实施（MVP 先打通一根管子）

1. **proto + build.rs**：拿旧版 OTLP `.proto`、生成 Rust、能 `cargo build`。
2. **convert + resource**：把 1 个指标（如 `veda_http_requests_total`）转成 `ExportMetricsServiceRequest`（attributes+labels 双写，resource 从 env.yaml）。
3. **discovery + send**：拉配置取 collector，gRPC 发一次，**在公司监控平台用 `appname=dbpaas-ai-service` 查到这个指标 = MVP 验证通过**。
4. **全量桥接**：解析 render() 全部指标 + 三类映射 + histogram 分桶。
5. **后台任务 + config**：5s 周期、轮询/重连、灰度开关。
6. **上机灰度**：`.161` dogfood 开 `[otlp] enabled=true`，对真实 Collector 跑一段，核对监控平台数据。

DoD：监控平台能稳定看到 veda 全部指标（维度正确、histogram 分布正确）；OTLP 故障不影响主服务（exporter 错误只 warn、不 panic、不阻塞）。

---

## 6. 开放问题 / 风险

- **proto 版本**：要精确拿到"含 `InstrumentationLibrary`+`labels`"的那版 `.proto`（v0.19+ 已删）。先从 cs-oss 仓库确认它实际编译用的 proto 文件，照抄最稳。
- **cumulative 语义**：Prometheus counter/histogram 本就是 cumulative，直接映射 `CUMULATIVE` 即可；gauge 直接取值。无需自己做 delta。
- **值类型**：counter/gauge 整型用 `as_int`，`_seconds` histogram 的 sum/bounds 用 `as_double`——映射时按指标判断。
- **Collector 连通**：`nc -z` 假阴性已知（见 reference memory），以 gRPC 实际 Export 返回 `OK` 为准。
- **env.yaml 不可读/格式变**：降级禁用 OTLP + warn，绝不影响主流程。
- **指标基数**：现有指标维度低（route/method/status/kind），无高基数风险；后续加指标守住低基数。

---

## 7. 工作量

proto+build ~0.5d；convert/resource/discovery/send ~1.5d；后台任务+config+灰度开关 ~0.5d；连真实 Collector 联调（MVP→全量）~1d。**合计 ~3.5d**。trace 二期另算。

---

## 8. Codex review 处置（2026-06-04, xhigh）

方向确认成立。已并入上文——**真问题**：#1 histogram 累积桶差分（§3.3）、#2 cumulative `start_time` 策略（§3.3）、#3 MVP 统一 `as_double`（§3.3）、#4 `[otlp]` config 字段补全 + env.yaml 降级（§3.6）；**边缘**：reqwest 提正式依赖（§4）、指标覆盖实际全集（§3.3）、proto pin SHA + vendored（§3.2）。**3 条"过度"= 确认核心决策成立**：不用 opentelemetry-rust、MVP 解析 text 不上 fanout、坚持 OTLP gRPC。
