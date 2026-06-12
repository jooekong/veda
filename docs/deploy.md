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
mkdir -p /data/veda/bin /data/veda/config
cp veda-server /data/veda/bin/ && cp server.toml /data/veda/config/
chown -R veda:veda /data/veda && chmod 600 /data/veda/config/server.toml

# systemd 双 unit(模板在 scripts/deploy/)
cp scripts/deploy/veda-server.{socket,service} /etc/systemd/system/
systemctl daemon-reload
systemctl enable --now veda-server.socket veda-server
```

关键项:
- `veda-server.socket`:`ListenStream=3000`,`Backlog=4096`(发布窗口的排队容量,
  受 `net.core.somaxconn` 上限约束,低流量下绰绰有余)
- `veda-server.service`:`Requires/After=veda-server.socket`;`TimeoutStopSec=120`
  盖住最慢 embedding batch(超时 SIGKILL 安全:outbox lease 会被重新认领,但
  in-flight 请求会断);`LimitNOFILE=65536`(压测 5k+ QPS 实证需要)
- `server.toml`:单节点 **`drain_secs` 保持 0(默认)**——drain 是多节点+ELB
  机制,单节点开了反而让 ELB 摘掉唯一后端

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
3. `server.toml` 配 `drain_secs = 10`:SIGTERM 后 `/v1/ready` 先 503
   `"draining"` 但继续服务,ELB 摘除本节点后才真正关闭(二次信号跳过等待)
4. 发布仍是同一个 `deploy.sh`,多传几个 host 即逐台滚动(带 ready 门禁 +
   观察期);此时 schema 必须严格 expand-contract(新旧版本真正共存)

所有节点 `worker.enabled = true` 即可:outbox 走 `FOR UPDATE SKIP LOCKED` +
10 分钟 lease,多实例并发消费安全。
