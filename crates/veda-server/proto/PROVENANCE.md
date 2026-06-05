# Vendored OTLP proto 来源

这些 `.proto` 是**手动 vendored 的旧版 OpenTelemetry OTLP proto**，veda 用 `tonic`+`prost`
自己生成 gRPC 代码，把 metrics 推到公司 Monitor Collector。**不要**换成官方 main 分支的新版
（见下方「为什么 pin 旧版」）。

## 来源链

1. 公司监控权威仓库：`git@git.ddxq.mobi:cs-oss/monitor-agent.git`
   - 分支 `origin/main`，commit **`7140424e7332fdf9da4eebd6f8565ddab33e6b15`**（2026-05-21）
   - 其 `docs/proto/csoss-monitor-otlp-entrypoint.proto` 是「非 Java SDK 编译入口」，它 import 了
     veda 需要的全部标准 OTLP 类型，并在 `docs/OTHER_LANGUAGE_SDK_INTEGRATION.md` 里点名
     依赖版本：`io.opentelemetry:opentelemetry-proto:1.4.1-alpha`。
2. 实际 `.proto` 文件取自该 jar：`io.opentelemetry:opentelemetry-proto:1.4.1-alpha`
   - jar 内 `.proto` 时间戳 2021-07-15，对应**仍含 `InstrumentationLibrary` + `StringKeyValue`(labels)**
     的旧版 spec。
   - 本地副本：`~/.m2/.../opentelemetry-proto/1.4.1-alpha/`，公司 nexus 也可下载
     （`https://nexus.ddxq.mobi/repository/maven-public/io/opentelemetry/opentelemetry-proto/1.4.1-alpha/`）。

## 为什么 pin 旧版（关键，别误升级）

公司 Collector 是**旧 receiver**，有两个非标准要求，官方新版 proto 满足不了：

- 用 `metrics.v1.InstrumentationLibraryMetrics`（**不是**新版 `ScopeMetrics`）——
  opentelemetry-proto **v0.19** 删掉了 `InstrumentationLibrary`。
- DataPoint 必须**双写** `attributes`(KeyValue) + deprecated `labels`(StringKeyValue)——
  opentelemetry-proto **v0.20** 删掉了 `labels`。Collector 当前读 `labels`，不双写就查不到数据。

所以 vendored 死这版、不跟随官方升级，是刻意的。升级前必须先确认 Collector 已支持
`ScopeMetrics`/纯 `attributes`。

## vendored 文件清单（一期 metrics）

```
opentelemetry/proto/common/v1/common.proto              # AnyValue/KeyValue/StringKeyValue/InstrumentationLibrary
opentelemetry/proto/resource/v1/resource.proto          # Resource
opentelemetry/proto/metrics/v1/metrics.proto            # ResourceMetrics/Metric/Sum/Gauge/Histogram/NumberDataPoint...
opentelemetry/proto/collector/metrics/v1/metrics_service.proto  # MetricsService/Export + Export*Request/Response
```

文件保持官方原样（含 Apache license 头），便于将来与官方 diff。`build.rs` 只编译
`metrics_service.proto` 入口，prost 顺 import 自动拉入其余三个。

二期 trace 需要时再从同一 jar 补 `trace/v1/trace.proto` + `collector/trace/v1/trace_service.proto`。
