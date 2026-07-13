# veda-tunnel 部署与发布

> 生产机：**10.79.52.95**（`tdchw-veda-tunnel-1.vmhwtl.ddmc-inc.com`，华为云 EulerOS 2.0，glibc **2.34**，4G/2C）。
> 定位：企微长连接桥（WSS ← 企微智能机器人 → veda `/v1/answer`+`/v1/search`）。
> **单实例铁律**：企微同一 bot_id 的新连接会踢掉旧连接——全网只允许一个 tunnel 进程在跑。起第二个实例（含本地调试连生产 bot）= 双方互踢风暴。

## 拓扑

```
企微用户 ⇄ 企微后台 ⇄ WSS openws.work.weixin.qq.com
                          ⇅ (长连接, tunnel 主动拨出)
              10.79.52.95  veda-tunnel
                 ├─ HTTP → 10.79.55.85:3000 (生产 veda-server, /v1/answer /v1/search)
                 ├─ MySQL → veda.mysql.srv.mc.dd/veda (veda_tunnel_bots, 与生产 veda-server 同库)
                 └─ admin 0.0.0.0:9110 (Bearer=生产 admin token)
                          ↑
  nginx (10.79.55.161) /tunnel/v1/ → 10.79.52.95:9110/admin/  ← 目标态，见「历史与注意」
    ├─ veda-prod.dbpaas.dingdongxiaoqu.com  → 管理页 #/admin/tunnel（生产 token 登录）
    └─ veda-test.dbpaas.dingdongxiaoqu.com  → 同一后端（页面会提示需生产 token）
```

- 机器布局：`/data/veda-tunnel/{bin,config,logs}`，配置 `config/tunnel.toml`（0600，含生产 MySQL DSN + admin token），日志走 journald（`journalctl -u veda-tunnel`）。
- systemd：`veda-tunnel.service`（`Restart=always`；无 socket activation——tunnel 是拨出方，没有入站监听要保）。

## bot 配置的三个入口（同一张表）

`veda_tunnel_bots`（生产 veda 库）是唯一事实源，谁写都行：

| 入口 | 路径 | 生效方式 |
|---|---|---|
| 生产 console UI | `veda-prod…/#/admin/tunnel` | tunnel admin API，**即时**生效 |
| tunnel admin API | `/tunnel/v1/bots`（nginx 反代 → `:9110/admin/bots`） | 即时（control loop） |
| **平台管理 API** | veda-server `/v1/workspace/{ws}/project/{id}/tunnel/bots` | tunnel **30s 轮询**收敛（直写共享表；见 `crates/veda-server/src/tunnel_bots.rs`） |

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

## 历史与注意

- 2026-07-13 前 tunnel 跑在 .161（测试机、连 veda_it 测试库）。迁移当天 .161 sshd 全天不可达（诊断：除 nginx 外 sshd/node_exporter/dogfood/旧 tunnel 全部死亡，疑似 OOM），迁移经旧 tunnel admin API 删 bot 完成切换，**三件收尾挂起，.161 恢复后必须做**：① `systemctl stop && systemctl disable veda-tunnel`（旧进程已死但 service 仍 enabled；veda_it 表留有 placeholder 行防 seed，重启拉起也无害，但要正式停用）② 两个入口 conf（`veda-alpha.conf` / `veda-prod.conf`，均在 `/etc/nginx/conf.d/`）的 `/tunnel/v1/` 从 `127.0.0.1:9110` 切到 `10.79.52.95:9110`（未切前 console 的 tunnel 页不可用，bot 管理走 .95 本机 admin API）③ 清掉 .161 tunnel.toml 里的 `[[wecom.bot]]` seed 段。**不要再在 .161 起 tunnel**（互踢）。
- 首次启动会自动 `CREATE TABLE IF NOT EXISTS` + 按 information_schema 补新列，无需手工建表。
- 升级 veda-server 平台 API（写共享表的那侧）与 tunnel 的顺序无要求——两边都带同一份幂等 schema 迁移。
