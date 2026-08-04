# 生产部署与平滑发布

> 当前拓扑(2026-06-12 拍板):**单 VM**(veda-server, systemd socket activation)→ MySQL / Milvus / airouter。
> 前置 ELB 可选(固定入口/TLS);流量增长后扩多节点的路径见文末,代码已就绪。
>
> 已知取舍:单节点的计划内发布是平滑的,但**计划外故障(VM 宕机/OOM/宿主机迁移)= 服务不可用直到人工恢复**。服务本身无状态(全在 MySQL/Milvus),恢复 = 任意新机器拉起进程。

## 平滑发布原理:systemd socket activation

listening socket 由 systemd 持有(`veda-server.socket` unit),服务进程通过
`LISTEN_FDS` 继承它(`main.rs` bind 处的 listenfd 分支;继承时忽略配置里的
`listen`,地址以 socket unit 为准):

1. **先换二进制(原子 rename),再 `systemctl restart`**。顺序不能反:socket
   unit 始终 active,service 停着时只要 backlog 来一个连接,systemd 就会按
   socket activation 语义自动拉起 service——若此时二进制还没换,起来的是旧
   版本,后续 `start` 变 no-op,ready 探活会在错误的版本上通过。先换后重启,
   任何路径拉起的都是新码。
2. restart 期间进程优雅关闭(处理完 in-flight 请求,worker 做完当前 batch),
   **socket 在 systemd 手里始终开着**——新建连接进内核 backlog 排队,不被
   拒绝,新进程起来后逐个 accept。
3. 发布窗口(graceful stop + 启动到 accept,实测约 2-4s)内:**新建连接零拒绝**,
   表现为一次延迟尖峰;存量 keep-alive 连接在优雅关闭时收到 FIN 正常断开
   (客户端连接池自动重建),仅当请求恰好与 FIN 撞上连接复用的瞬间才会失败——
   窗口极小,且这是任何发布方式(包括 LB drain)共有的 HTTP keep-alive 固有
   race。crash 后 `Restart=always` 拉起期间同样享受 backlog 排队。

发布操作就是换二进制 + `systemctl restart`,没有编排、没有 LB 联动、没有 a/b 状态。

## 安装(新 VM provision)

```bash
# 专用低权限用户(服务持有 DB/API 凭证且面向网络,不以 root 跑)
useradd -r -s /sbin/nologin -d /data/veda veda

# 二进制与配置(CI 不发 server 产物,在同构 glibc 的 build 机上编译)
mkdir -p /data/veda/bin /data/veda/config /data/veda/data   # data 必须建:unit 里
                                                            # ProtectSystem=strict +
                                                            # ReadWritePaths=/data/veda/data
                                                            # (无 `-` 前缀),目录不存在直接起不来
cp veda-server /data/veda/bin/
cp config.toml /data/veda/config/          # 从 config/server.toml.example 抄改而来
chown -R veda:veda /data/veda && chmod 600 /data/veda/config/config.toml

# systemd 双 unit(模板在 scripts/deploy/)
cp scripts/deploy/veda-server.{socket,service} /etc/systemd/system/
# ⚠️ 改 ExecStart 的配置路径——见下面「配置」一节
systemctl daemon-reload
systemctl enable --now veda-server.socket veda-server
```

关键项:
- `veda-server.socket`:`ListenStream=3000`,`Backlog=4096`(发布窗口的排队容量,
  受 `net.core.somaxconn` 上限约束,低流量下绰绰有余)
- `veda-server.service`:`Requires/After=veda-server.socket`;`TimeoutStopSec=120`
  盖住最慢 embedding batch(超时 SIGKILL 安全:outbox lease 会被重新认领,但
  in-flight 请求会断);`LimitNOFILE=65536`(压测 5k+ QPS 实证需要)
- `config.toml`:单节点 **`drain_secs` 保持 0(默认)**——drain 是多节点+ELB
  机制,单节点开了反而让 ELB 摘掉唯一后端

## 配置

**配置路径是部署选择,不是写死的**。二进制只吃一个位置参数;省略时默认
`config/server.toml`(相对 cwd)。**现有节点(.161 / .89 / .85)统一用
`/data/veda/config/config.toml`**,本文所有命令按这个约定写。

> ⚠️ **模板路径不一致,拷完要改**:`scripts/deploy/veda-server.service` 的
> `ExecStart` 写死 `/data/veda/config/server.toml`,与上述节点约定不符。拷进
> `/etc/systemd/system/` 后必须把 `ExecStart` 改成你实际选的路径,否则服务起来
> 读不到配置。**只有 .service 要改**——`scripts/deploy/veda-server.socket` 里
> 不含任何配置路径,只有一句"`listen` in server.toml is ignored"的注释(说的是
> 被忽略的配置项,不是文件路径),不用动。
> (`deploy/systemd/veda-server.service` 那份已经指向 `config.toml`,但它是
> "原生 MySQL+Milvus 单 unit"变体,不带 socket activation。)

### 命令行契约

```
veda-server [config.toml]
```

- **一个位置参数** = 配置路径;省略 → `config/server.toml`
- `--help` / `-h`:往 stderr 打 `Usage: veda-server [config.toml]`,退出码 0
- `--version` / `-V`:往 stdout 打 `veda-server <crate 版本>`,退出码 0。**只此
  两个 flag**,其他任何 `--flag` 是硬错误(`unknown flag: {other}`),进程直接退出
- **`--version` 报的是 crate 版本,不等于 build**:节点上跑未发版 commit 时它照样
  显示上一个 tag。它能证伪(输出对不上发版号 = 换错了),不能证明。**线上也没有
  任何版本端点**:`GET /capabilities` 只返回 `{"summary_enabled": bool}`,MCP
  `initialize` 响应的 `serverInfo.version` 同样是 crate 版本、且要 `wk_` 才能拿。
  想确认线上跑的是哪个 build,**核对二进制 sha256**(runbook 的 swap 流程本来
  就在做,见 [`docs/deploy-runbook.md`](deploy-runbook.md) 的「同一个 binary
  复用到所有节点 swap」一节)
- 启动时自动跑 schema bootstrap(`CREATE TABLE IF NOT EXISTS`,幂等),没有独立
  的 migrate 二进制或 unit

### 必填项与生产必开项

全量 key + 默认值见 [`config/server.toml.example`](../config/server.toml.example)
(新建配置就从它抄)。

**必填**——这几项没有 serde default,缺任何一个都是启动即解析失败:

- `mysql.database_url`
- `milvus.url`
- `embedding.api_url` / `api_key` / `model` / `dimension`

**生产必须显式打开**——默认全是关的,失败大多是静默的(404 / 拒绝,不是报错);
唯一的例外是最后一行的 `VEDA_PLATFORM_BASE`,它**静默放行**:

| key | 不配的后果 |
| --- | --- |
| `metrics_token` | `/v1/metrics` 和 `POST /admin/v1/reconcile/{workspace_id}` 双双 404 |
| `admin_token` | 只挡 `/admin/v1/workspaces*` 那六条路由(404),console 因此失效;`/admin/v1/reconcile/*` 吃 `metrics_token`、`/admin/v1/tokens*` 吃 `vk_` 账号鉴权,两者都不受影响 |
| `allowed_origins` | 空 + `dev_mode = false`(默认)→ 拒绝所有跨域浏览器请求 |
| `[otlp]` | 零指标上报 |
| `VEDA_PLATFORM_BASE`(env,不是 TOML key) | ⚠️ **fail-open,与上面几项相反**:未配时 `platform::authorize()` 无条件 `Ok(())`,整个 `/v1/workspace/**` 平台面**静默跳过鉴权**。不配不是"关掉功能",是"敞开大门" |

> tunnel 的 admin 面不在这张表里:veda-tunnel 是**独立进程、独立端口**(生产
> 9110),`/admin/*` 读它自己 `tunnel.toml` 的 `[admin].token`,与 server 的
> `admin_token` 无关(见 [`docs/deploy-tunnel.md`](deploy-tunnel.md));server 侧
> 那几条 `/v1/workspace/{ws}/project/{id}/tunnel/*` 走网关身份,也不吃
> `admin_token`。

凭证类的值可以走 `EnvironmentFile=`(样例 `deploy/systemd/veda.env.example`),
但**上面让你拷的 `scripts/deploy/veda-server.service` 里并没有这行**——它只有
`Environment=RUST_LOG=info`。要用就自己补
`EnvironmentFile=-/data/veda/config/veda.env`(写法照抄
`deploy/systemd/veda-server.service:27`,那份非 socket-activation 变体带了)。

`VEDA_*` env **只覆盖一部分配置项,且名字不等于 TOML 路径**——例如
`mysql.database_url` 对应的是 `VEDA_MYSQL_URL`,不是 `VEDA_MYSQL_DATABASE_URL`。
权威清单看 [`config/server.toml.example`](../config/server.toml.example) 的行内
注释:标了 `VEDA_*` 的才有 env 覆盖。**`[worker]`、`[llm]` 的调参项
(`max_summary_tokens` / `answer_*`)、`milvus.db` 一个 env 覆盖都没有,只能改
TOML。**

## 发布

```bash
cargo build --release -p veda-server   # build 机,与 VM 同构 glibc
scripts/deploy/deploy.sh target/release/veda-server <vm>
```

脚本:推二进制 → 旧版本留 `.bak` → 原子换 → `systemctl restart`(排队窗口)→
等 `/v1/ready` 200。超时即中止并打印**回滚命令**(`mv .bak 回去 + restart`);
也可用同一脚本直接发任意旧二进制。

## 验证

- drain/优雅关闭集成测试(spawn 真二进制 + 真 SIGTERM + 真存储):

  ```bash
  NO_PROXY='*' cargo test -p veda-server --test graceful_drain_test -- --ignored --nocapture
  ```

- socket activation 继承路径,在任意有 systemd 的机器上:

  ```bash
  # 这里的 config/server.toml 是 repo 内的本地配置(= 二进制省略参数时的默认路径),
  # 与线上 /data/veda/config/config.toml 是两回事
  systemd-socket-activate -l 3000 ./veda-server config/server.toml
  # 另一终端: curl localhost:3000/v1/ready → 200,日志应打 "inherited socket"
  ```

  (mac 本地已用等效 LISTEN_FDS 模拟验证过:继承端口服务正常、`listen` 配置
  被忽略、SIGTERM 干净退出。)

## schema 约束

单节点发布窗口内只有一个版本在跑,migrate(启动时自动、幂等)没有共存问题。
但 **SIGKILL 兜底 / 回滚旧二进制**意味着:旧代码可能跑在新 schema 上——schema
变更仍应保持加法(加列带默认值/加索引/加表),删列改列先确认不再回滚到依赖它
的版本。

## 扩容路径(流量上来后)

多节点 + ELB 的全套已就绪,切换成本只有配置:

1. 加 VM,同样方式 provision(socket activation 保留,无害且继续兜底单机重启)
2. 挂 **应用型(7 层)ELB**,健康检查 `GET /v1/ready` 期望 200;
   检查间隔 × 不健康阈值 **<** `drain_secs`(如 2s×3=6s < 10s);
   上线前确认 ELB"全部后端不健康"时是 fail-open 还是黑洞
3. `config.toml` 配 `drain_secs = 10`:SIGTERM 后 `/v1/ready` 先 503
   `"draining"` 但继续服务,ELB 摘除本节点后才真正关闭(二次信号跳过等待)
4. 发布仍是同一个 `deploy.sh`,多传几个 host 即逐台滚动(带 ready 门禁 +
   观察期);此时 schema 必须严格 expand-contract(新旧版本真正共存)

⚠️ **worker 不能多实例**。d94bd20(2026-07 单 pod 简化)删掉了 outbox 的
per-owner(`lease_owner` = host:pid)fencing——理由正是"这套部署从来没跑过多
server,这层保护是白付的复杂度"。`FOR UPDATE SKIP LOCKED` 和 10 分钟 lease
仍然在(`crates/veda-store/src/mysql.rs`),但 `complete` / `fail` / 心跳这些
lifecycle 调用现在**只按 `status = 'processing'` 兜底**,幂等性靠 content-hash
watermark 兜——即"偶发重复执行不脏数据",不是"并发消费安全"。

所以扩多节点时:**同一个数据库永远只跑一个 worker**——只在一台配
`worker.enabled = true`,其余全部 `false`(纯无状态读写节点)。注意 **`[worker]`
没有任何 `VEDA_*` env 覆盖,只能逐台改 `config.toml`**,别指望用环境变量批量关。
真要多 worker 并发消费,先把 per-owner fencing 加回来。
