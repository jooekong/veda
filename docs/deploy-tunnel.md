# veda-tunnel 部署与发布

> 生产机：**10.79.52.95**（`tdchw-veda-tunnel-1.vmhwtl.ddmc-inc.com`，华为云 EulerOS 2.0，glibc **2.34**，4G/2C）。
> 测试机：**10.79.55.89**（与测试 veda-server 同机，2026-07-14 起）。
> 定位：企微长连接桥（WSS ← 企微智能机器人 → veda `/v1/answer`+`/v1/search`）。
> **单连接铁律**：企微**同一 bot_id** 的新连接会踢掉旧连接。生产（.95→生产库）/ 测试（.89→测试库）各跑一个实例、各管各库，互不相干；但**同一个 bot_id 绝不能同时配进两个库**（= 互踢风暴），本地调试也不要连任何已被实例接管的 bot。

## 拓扑

```
企微用户 ⇄ 企微后台 ⇄ WSS openws.work.weixin.qq.com
                          ⇅ (长连接, tunnel 主动拨出; 生产/测试各自拨出, bot_id 不重叠)
  ┌─ 生产 10.79.52.95  veda-tunnel
  │    ├─ HTTP → 10.79.55.85:3000 (生产 veda-server, /v1/answer /v1/search)
  │    ├─ MySQL → veda.mysql.srv.mc.dd/veda (veda_tunnel_bots, 与生产 veda-server 同库)
  │    └─ admin 0.0.0.0:9110 (Bearer=生产 admin token)
  └─ 测试 10.79.55.89  veda-tunnel
       ├─ HTTP → 127.0.0.1:3000 (测试 veda-server 本机)
       ├─ MySQL → 10.78.81.148/veda (veda_tunnel_bots, 与 .89 测试 server 同库)
       └─ admin 0.0.0.0:9110 (Bearer=测试 admin token, 与 .89 VEDA_ADMIN_TOKEN 一致)

  nginx (10.79.51.161, tdct-dbpaas-ai-service-4) 两入口各指各的 tunnel：
    ├─ veda-prod.dbpaas…/tunnel/v1/ → 10.79.52.95:9110/admin/（生产 token 登录）
    └─ veda-test.dbpaas…/tunnel/v1/ → 10.79.55.89:9110/admin/（测试 token 登录, veda-alpha.conf）
```

- 机器布局：`/data/veda-tunnel/{bin,config,logs}`，配置 `config/tunnel.toml`（0600，含生产 MySQL DSN + admin token），日志走 journald（`journalctl -u veda-tunnel`）。
- systemd：`veda-tunnel.service`（`Restart=always`；无 socket activation——tunnel 是拨出方，没有入站监听要保）。

## bot 配置的三个入口（同一张表）

`veda_tunnel_bots` 是唯一事实源（生产库 / 测试库各一张，互不相干），谁写都行：

| 入口 | 路径 | 生效方式 |
|---|---|---|
| console UI | `veda-prod…/#/admin/tunnel`（生产）/ `veda-test…/#/admin/tunnel`（测试） | tunnel admin API，**即时**生效 |
| tunnel admin API | `/tunnel/v1/bots`（nginx 反代 → 各自 `:9110/admin/bots`） | 即时（control loop） |
| **平台管理 API** | veda-server `/v1/workspace/{ws}/project/{id}/tunnel/bots` | tunnel **30s 轮询**收敛（直写共享表；见 `crates/veda-server/src/tunnel_bots.rs`）——测试平台建的 bot 由 .89 实例接管，生产的由 .95 接管 |

平台 API 建的 bot 自动 mint 只读 `wk_`（删除时同步 revoke）；tunnel 每 30s 把连接状态心跳（`conn_state`/`conn_updated_at`）写回表里，平台 GET 直接可见——两个进程之间零 RPC，全靠这张表。

表结构 owner = `crates/veda-tunnel/src/store.rs`（server 侧 `tunnel_bots.rs` 复制同一份 DDL+列迁移，两边谁先启动都能建表/补列；改列必须两处同步）。

## 发布流程

CI 不发 tunnel 产物（同 server）。**.95 是 glibc 2.34，二进制必须在 .89 上编译**（.161 是 2.38，产物在 .95 跑不起来）。boxes 之间不能互 SSH，经 Mac 中继：

```bash
# 1) .89 上 build（源码树 /root/veda-build 先 rsync 到目标 sha）
ssh root@10.79.55.89 'cd /root/veda-build && nohup cargo build --release -p veda-tunnel > /tmp/tunnel-build.log 2>&1 & echo started'
# 轮询 target/release/veda-tunnel 出现即完成（依赖已缓存时 ~1-2min）

# 2) Mac 中继到 .95
scp root@10.79.55.89:/root/veda-build/target/release/veda-tunnel /tmp/vt
scp /tmp/vt root@10.79.52.95:/tmp/veda-tunnel.new && rm /tmp/vt

# 3) .95 swap + 重启（先备份；无 socket activation，直接 restart，
#    WSS 断开期间企微消息不丢——企微侧会在重连后按会话续投）
ssh root@10.79.52.95 'cp /data/veda-tunnel/bin/veda-tunnel /data/veda-tunnel/bin/veda-tunnel.bak \
  && install -m 755 /tmp/veda-tunnel.new /data/veda-tunnel/bin/veda-tunnel \
  && systemctl restart veda-tunnel && sleep 3 && systemctl is-active veda-tunnel \
  && journalctl -u veda-tunnel -n 5 --no-pager && rm /tmp/veda-tunnel.new'
```

### 验证

```bash
# 进程 + 订阅成功（每个 bot 一条 subscribed）
ssh root@10.79.52.95 'journalctl -u veda-tunnel --since "-2min" --no-pager | grep -E "subscribed|error"'
# admin 面 fail-closed：无 token 401
ssh root@10.79.52.95 'curl -s -o /dev/null -w "%{http_code}\n" http://127.0.0.1:9110/admin/bots'
# 经 nginx 入口（生产 token）
# 浏览器打开 veda-prod…/#/admin/tunnel，bot 徽标应为「在线」
# 最后企微里 @bot 问一句真题
```

### 回滚

```bash
ssh root@10.79.52.95 'cp /data/veda-tunnel/bin/veda-tunnel.bak /data/veda-tunnel/bin/veda-tunnel && systemctl restart veda-tunnel'
```

### 测试实例（.89）发布

build 机就是部署机，无需中继：

```bash
ssh root@10.79.55.89 'cd /root/veda-build && cargo build --release -p veda-tunnel \
  && cp /data/veda-tunnel/bin/veda-tunnel /data/veda-tunnel/bin/veda-tunnel.bak \
  && install -m 755 target/release/veda-tunnel /data/veda-tunnel/bin/veda-tunnel \
  && systemctl restart veda-tunnel && sleep 3 && systemctl is-active veda-tunnel'
```

机器布局与 .95 相同（`/data/veda-tunnel/{bin,config,logs}` + systemd `veda-tunnel.service`）。验证同上，入口换 `veda-test…/#/admin/tunnel`（测试 token）。

## 配置文件（/data/veda-tunnel/config/tunnel.toml）

```toml
veda_base_url = "http://10.79.55.85:3000"   # 生产 veda-server 内网直连

[mysql]
database_url = "mysql://rw_veda:<密码>@veda.mysql.srv.mc.dd:3306/veda"  # 与 .85 server 同库

[admin]
listen = "0.0.0.0:9110"   # 非 loopback：nginx 在 .161 跨机反代；鉴权靠 token + 内网安全组
token = "<生产 admin token，与 .85 veda.env 的 VEDA_ADMIN_TOKEN 一致>"

[answer]
enabled = true            # 改动需重启（进程启动时读一次）
```

凭证来源：MySQL DSN / admin token 都在 .85 的 `/data/veda/config/`（`config.toml` + `veda.env`）——需要时从那里取，**不要经聊天/commit 传递**。

测试实例（.89）的差异：`veda_base_url = "http://127.0.0.1:3000"`；DSN / admin token 取自 .89 本机 `/data/veda/config/`（与测试 veda-server 同库同 token）；其余相同。

## 历史与注意

- 2026-07-13 前 tunnel 跑在 .161（入口机、连 veda_it 测试库）；迁移当天已收尾：旧 `veda-tunnel.service` stop+disable、tunnel.toml 的 `[[wecom.bot]]` seed 段注释（另有 veda_it 表 placeholder 行双保险）、nginx 双入口 conf 切 `10.79.52.95:9110`（备份 `.bak-tunnel95`）。**不要再在 .161 起 tunnel**。
- 2026-07-14 增设测试实例于 .89（平台同事在测试环境建的 bot 此前永远「等待连接」——无实例读测试库）；nginx `veda-alpha.conf` 的 `/tunnel/v1/` 切 `10.79.55.89:9110`（备份 `.bak-tunnel-test89`），veda-test console 自此用**测试** token 管理。二进制直接复用 .89 build 产物（与 .95 生产同 md5）。
- **⚠️ .161 的真实 IP = `10.79.51.161`**（`tdct-dbpaas-ai-service-4`，51 网段）——`.89/.85` 是 `10.79.55.x`，但 .161 不是！迁移当天曾按 `10.79.55.161` 连了一天连不上并误诊为整机故障（那是台不相干的机器）。SSH 走 inner-gw 跳板。
- 首次启动会自动 `CREATE TABLE IF NOT EXISTS` + 按 information_schema 补新列，无需手工建表。
- 升级 veda-server 平台 API（写共享表的那侧）与 tunnel 的顺序无要求——两边都带同一份幂等 schema 迁移。
