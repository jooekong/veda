# Veda 发布 Runbook(实操流程)

> 本文是**照着执行的操作手册**;平滑发布的原理(systemd socket activation)见 [`docs/deploy.md`](deploy.md)。
> 节点细节见记忆 `reference_prod_box`(.161/.89)、`reference_prod_node_85`(.85)。
> 沉淀自 2026-06-24 的 0.1.15 发布(blob 存储 + PDF 提取)——那次的踩坑见文末附录,已固化成下面的「铁律」。

---

## 0. 环境拓扑

| 节点 | 角色 | 后端 | 源码/build | glibc | HTTP 入口 |
|---|---|---|---|---|---|
| **10.79.51.161** | 测试·dogfood + nginx 公网入口 | test 共享 MySQL/Milvus | `/data/rust/veda`(**有 git**) | 2.38 | nginx → veda-test 域名 + 反代 .89 |
| **10.79.55.89** | 测试·friends alpha | test 共享 MySQL/Milvus | `/root/veda-build`(**无 git**,rsync) | 2.34 | 经 .161 nginx |
| **10.79.55.85** | **生产**(独立) | **生产**独立 MySQL/Milvus | `/root/veda-build`(**无 git**,rsync) | 2.34 | 3000 仅内网(SSH 隧道) |

- **CI 不发布 veda-server**,server 一律在节点上从源码 build(CLI 才走 CI artifacts)。
- 三台 SSH 互不相通,机器间传文件经 Mac 中继。

---

## 铁律(违反就会出事,2026-06-24 实测教训)

1. **远程 build 必须 detached**,且 SSH 全程带 keepalive:
   ```bash
   ssh -o ServerAliveInterval=20 <node> 'cd <build+dir> && nohup cargo build --release -p veda-server > /tmp/vb.log 2>&1 </dev/null & echo started'
   ```
   ❌ 禁止 `ssh <node> 'cargo build ...'` 前台跑。客户端 SSH 超时断开时,**服务端 sshd 未必同步察觉**,cargo 继续跑成孤儿,一堆 rustc 吃满 CPU → sshd 握手都超时 + nginx 502,整台拖垮(.161 当场中招)。
2. **不要对同一节点频繁串行 SSH**——`.161` 的 sshd 会触发 rate-limit(banner-exchange timeout)。把多个检查/操作**合并进一条 SSH**。连接异常时先 `rm ~/.ssh/master-<node>*` 清 stale ControlSocket。
3. **build 一次,复用到所有节点**:**统一在一台 glibc 2.34 节点(`.89`)build 一次发版 tag,得到的同一个 binary 部署到所有节点**(`.89`/`.85` 都 2.34 直接用,`.161` 2.38 向后兼容)。同一个文件 = 版本绝对一致,且只编一次(成本最低)。binary 是纯代码、配置在各节点的 `config.toml`,所以测试/生产复用同一个 binary 安全(不存在环境差异)。**glibc 高的 binary 不能往低跑**——别在 `.161`(2.38)上 build,2.38 binary 上不了 `.89`/`.85`(2.34)。
4. **生产最后上**:测试节点(`.161`/`.89`)部署并验证通过后,再上生产 `.85`(灰度,不要并行)。
5. **每次 swap 必走**:`backup → mv 原子 rename → restart`(别用 `cp`/`install` 直接覆盖运行中 binary),且 swap 后**必须验证**(下方清单)。
6. **生产 swap 有 downtime**(socket activation 下新连接 backlog 排队、不拒绝,约 2–5s 延迟尖峰)——选低峰执行;留好 backup 以便秒级回滚。

---

## 阶段 1 — 代码合并 + 发版(本机 Mac)

```bash
cd <repo>
# 1.1 合并到 main(feat 基于 main,应是 fast-forward)
git checkout main && git merge --ff-only feat/<branch>
git push origin main && git push ddxq main

# 1.2 发版:bump 版本 + 打 tag。同版本号会在第二个(gitlab)tag 处按设计 abort,属正常。
./scripts/release.sh X.Y.Z X.Y.Z
#   → 产生 "chore: bump version" commit + 本地 tag X.Y.Z,在 gitlab tag 处 abort
# 1.3 手动补 push(release.sh 在 abort 前没 push)
git push origin main && git push origin X.Y.Z
git push ddxq  main && git push ddxq  X.Y.Z   # ← gitlab tag 触发 CLI CI build

# 1.4 确认 GitLab CLI 发布完成(CI 不发 server,只发 CLI)
# 新版 glab 表格前有两行说明,head -3 只剩表头看不到 pipeline 行(0.1.21 发版实测踩坑)
NO_PROXY=git.ddxq.mobi glab ci list -R middleware/dbpaas/veda | head -8   # 0.1.x 行应 (success)
TOKEN=$(grep -oE 'GITLAB_DEPLOY_TOKEN="[^"]+"' install.sh | cut -d'"' -f2)
curl -s --noproxy '*' --header "Deploy-Token: $TOKEN" \
  https://git.ddxq.mobi/api/v4/projects/9462/packages/generic/veda/latest/LATEST_VERSION
#   → 应回显新版本号 = 用户 `install.sh|sh` 能装到
```

---

## 阶段 2 — Server 部署:build 一次,复用到所有节点

> **核心**:阶段 1 已发版定 tag。**只 build 一次**(发版 tag 的源码,在 glibc 2.34 节点 `.89`),得到**一个 binary,部署到所有节点**(测试 + 生产)。同一个文件 ⇒ 版本绝对一致,且只编一次(成本最低)。
> 为什么测试/生产能复用同一个 binary:binary 是**纯代码**,各节点跑自己的 `config.toml`(后端地址/密码在 config、不在 binary),所以同一个 2.34 binary 在 `.89`(测试库)和 `.85`(生产库)都正确——配置隔离,代码相同。glibc 2.34 向后兼容,`.161`(2.38)也能跑。

### 2A. build 一次(发版 tag,在 `.89`,glibc 2.34)

```bash
TAG=0.1.15   # 阶段 1 发版确定的版本

# 同步该 tag 全树源码(排除 target/.git;git archive 给旧 mtime,故传完 touch 防 cargo 跳过重编)
TMP=$(mktemp -d); git archive "$TAG" | tar -x -C "$TMP"
rsync -a --checksum --delete --exclude=/target --exclude=/.git -e "ssh -o ServerAliveInterval=20" "$TMP"/ 10.79.55.89:/root/veda-build/
rm -rf "$TMP"
# ⚠️ Mac 自带 rsync 是 openrsync,其 --checksum 的 dry-run itemize 有假阳性(2026-07-30 实测
# 338 个文件误报 11 个)。传输本身可用,但**源码一致性判据**要用两侧 sha256 manifest 对比,别信 dry-run。
ssh -o ServerAliveInterval=20 10.79.55.89 'find /root/veda-build \( -type d \( -name target -o -name .git \) \) -prune -o -type f -print0 | xargs -0 touch'

# detached build(铁律 1),后台等完成(新依赖首次约 9min,纯增量更快)
ssh -o ServerAliveInterval=20 10.79.55.89 'cd /root/veda-build && nohup ~/.cargo/bin/cargo build --release -p veda-server > /tmp/vb.log 2>&1 </dev/null & echo started'

# 等 Finished 后【三项校验】——这个 binary 要上生产,务必校准
ssh -o ServerAliveInterval=20 10.79.55.89 '
  B=/root/veda-build/target/release/veda-server
  objdump -T $B | grep -oE GLIBC_[0-9.]+ | sort -V | tail -1   # ① 须 <= 2.34
  grep -m1 "^version" /root/veda-build/Cargo.toml              # ② 须 = TAG(确认编的是发版源码,非残留)
  $B --help >/dev/null && echo "③ smoke ok"
'
```

> **复用 invariant(发布前自检)**:复用同一 binary 安全的前提是 **binary 里没有 per-环境的值**。binary = 代码 + `include_str!` embed 的 `install.sh`(内容固定、非 per-环境)+ `CARGO_PKG_VERSION`;**per-环境的后端地址/密码/token 都在各节点 `config.toml`、不在 binary**。自检:`rg 'env!|option_env!' crates/veda-server/src crates/veda-core/src`(只看这两处 src,test 里的 `CARGO_BIN_EXE` 可忽略)应只命中 `CARGO_PKG_VERSION` 类编译期常量。当前满足(Codex 2026-06 核实)。**谁要往 server 加 `env!`/`option_env!` 读环境,必须先废掉本复用方案。**

### 2B. 同一个 binary 复用到所有节点 swap(测试先,生产后)

三台 SSH 互不相通,binary 经 Mac 中继传到每个目标节点的 `.new`(放**目标同目录**,确保下面 `mv` 是同文件系统的原子 rename;`.89` 自己就地):
```bash
scp -o ServerAliveInterval=20 10.79.55.89:/root/veda-build/target/release/veda-server /tmp/vs-new   # 拉到 Mac
sha256sum /tmp/vs-new                                                              # 记下 hash,各节点核对是同一个 binary
scp -o ServerAliveInterval=20 /tmp/vs-new <node>:/data/veda/bin/veda-server.new     # 推到 .161 / .85 的同目录
# .89 就地:cp /root/veda-build/target/release/veda-server /data/veda/bin/veda-server.new
```

每节点统一 swap + 验证(**swap-first:先原子 rename 换 binary,再 restart——socket activation 的硬要求,顺序不可反**;对齐 `scripts/deploy/deploy.sh`):
```bash
ssh -o ServerAliveInterval=20 <node> '
  B=/data/veda/bin/veda-server
  sha256sum $B.new                              # 核对 = Mac 上记的 hash(确认是同一个 binary)
  chmod 755 $B.new
  [ -f $B ] && cp -p $B $B.bak.$(date +%s)      # backup(回滚用)
  mv -f $B.new $B                               # 原子 rename(运行进程保留旧 inode,不受影响)
  systemctl restart veda-server                 # 先换后重启:任何路径拉起的都是新码
  sleep 3
  echo "healthz: $(curl -s localhost:3000/healthz)"
  echo "ready:   $(curl -s localhost:3000/v1/ready)"
'
```

> 远程多行脚本用 `ssh <node> 'bash -s' < script.sh` 传;**别把 heredoc 接在管道后面**——
> `ssh ... 2>&1 | tee log <<'EOS'` 会把 heredoc 喂给 `tee` 而不是 `ssh`,远端 `bash -s` 挂起等输入直到超时(2026-07-30 踩坑,幸好只读核查确认零副作用)。

**顺序(铁律 4)**:先 `.161` + `.89`(测试)→ 走阶段 3 验证 → **通过后**再 `.85`(生产)。生产复用的就是测试已验过的**同一个 binary**,确定性最高。
- `.161`(有 git)若不想等 scp,也能 `git fetch && git checkout $TAG` 本地 build,但同为测试环境,复用 `.89` 的 binary 更省。

---

## 阶段 3 — 验证清单(每节点 swap 后必跑)

```bash
ssh <node> '
  curl -s localhost:3000/healthz                       # → ok
  curl -s localhost:3000/v1/ready                       # → mysql ok + milvus ok
  # blob 行为探活(确认是新代码:旧码 PUT 二进制返回 400)
  WK=$(curl -s -X POST localhost:3000/v1/accounts/anonymous | grep -oE "wk_[A-Za-z0-9_]+" | head -1)
  printf "%%PDF-1.4\x00\xff\xc0probe" > /tmp/p.bin
  curl -s -o /dev/null -w "PUT %{http_code}\n" -X PUT -H "Authorization: Bearer $WK" --data-binary @/tmp/p.bin localhost:3000/v1/fs/p.bin   # → 200
  curl -s -H "Authorization: Bearer $WK" localhost:3000/v1/fs/p.bin | cmp - /tmp/p.bin && echo "roundtrip ok"
  curl -s -o /dev/null -X DELETE -H "Authorization: Bearer $WK" localhost:3000/v1/fs/p.bin   # 清理探活文件(生产尤其要清)
'
```

- 全端点回归(可选):本机 `NO_PROXY='*' VEDA_BASE_URL=https://<部署入口> cargo test -p veda-server --test remote_e2e_test -- --ignored --test-threads=1`(应全 passed;打的是部署的 endpoint,不是本地代码)。
- 公网入口确认:`curl --noproxy '*' https://veda.dbpaas.dingdongxiaoqu.com/healthz` → 200。

**验证清单(每节点逐项打勾)**:
- [ ] healthz = ok
- [ ] ready:mysql + milvus 都 ok
- [ ] blob 探活:PUT 二进制 200 + roundtrip 无损 + 探活文件已清
- [ ] 生产额外:无新 dead-letter(`SELECT status,COUNT(*) FROM veda_outbox GROUP BY status`)、RSS/连接数正常

---

## 回滚

**binary 回滚也走 swap-first**(❌ 别先 `systemctl stop`——停了之后 socket activation 一来连接就用**当前磁盘上的 binary** 拉起,若还没换回去拉起的就是坏版本):
```bash
ssh -o ServerAliveInterval=20 <node> '
  B=/data/veda/bin/veda-server
  cp -p $B.bak.<ts> $B.new && mv -f $B.new $B   # 原子换回备份
  systemctl restart veda-server
  sleep 2; curl -s localhost:3000/healthz
'
```

**数据兼容性(回滚前必查,比换 binary 更要命)**:0.1.15 写入了 `storage_type='blob'` 文件 + `extract_sync` outbox 事件,**只有含 blob 代码的构建才认得**。
- ✅ **可回滚到**:含 blob 代码的任意构建——当前只有 `0.1.15`(`a439167`)这条线,以及测试节点跑的 blob commit `f8d792b`(代码含 blob,只是 version 字段还显示 0.1.14)。
- ❌ **绝不可回滚到**:`tag 0.1.14`(`e1039e0`,**pre-blob**,`StorageType` 只有 Inline/Chunked)、`0.1.13` 等任何**早于 blob 的发版** —— 旧码读 blob 文件报 `unknown storage_type: blob`、worker 消费 `extract_sync` 失败,表面起来了实则数据读不出、outbox 卡死。
- **⚠️ 别被 version 号骗了**:`tag 0.1.14` 是 pre-blob,而测试节点那个"0.1.14"是 blob commit `f8d792b`——**回滚认「代码是否含 blob」,不认 version 号**。
- 回滚前 gate check(两条任一 > 0 即禁止退 pre-blob):
  ```sql
  SELECT COUNT(*) FROM veda_files  WHERE storage_type='blob';                                  -- 已有 blob 文件
  SELECT COUNT(*) FROM veda_outbox WHERE event_type='extract_sync' AND status IN ('pending','processing');  -- 待消费的 extract 事件
  ```
  任一 > 0 → **只能 roll-forward**(发修复版),不能退 pre-blob。`veda_file_blobs` 表留着无害,致命的是**数据格式**旧码不认。

**退到 d94bd20 之前的 binary(单 pod 简化)**:d94bd20 的 migrate 会 `ALTER TABLE veda_outbox DROP COLUMN lease_owner`。schema 本身可自愈——旧码启动时自己会把这个可空列加回来——但**退版瞬间处于 `processing` 的行 `lease_owner` 是 NULL**,而旧码的 `complete()`/`fail()` 都 fence 在 `WHERE lease_owner = ?` 上,这些行会 no-op 卡住,要等 10 分钟 lease 过期才被重新 claim。
- 退版前 gate check:
  ```sql
  SELECT COUNT(*) FROM veda_outbox WHERE status='processing';   -- 为 0 再退
  ```
  非 0 就先 stop server 等 worker 把在途任务跑完(或直接等满 10 分钟 lease),再换 binary。
- 反向同理:**升到 d94bd20 是 stop-then-start,不兼容滚动发布**——DROP COLUMN 期间不能有旧码进程还在按 `lease_owner` fence。

---

## 生产 `.85` 专项注意

- **3000 仅内网**:验证从 SSH 进节点本地 curl,或 Mac 走 SSH 隧道;别指望 Mac 直连。
- **Milvus 建表权限**:新部署若引入新 collection,`rw_veda` 需要 create 权限(历史踩过 `PrivilegeCreateCollection`)。blob/PDF **不建新 collection**(PDF 进现有 `veda_chunks`),无此风险。
- **MySQL migrate 权限(预检)**:`migrate()` 不止 `CREATE TABLE`——历史迁移含 `ALTER TABLE ADD COLUMN/INDEX` + backfill `UPDATE`;**服务账号缺 ALTER/INDEX/UPDATE 权限会在启动 migrate 时直接失败 = 服务起不来**。换库 / 新部署前,用服务账号跑一次 DDL 权限 smoke:
  ```sql
  CREATE TABLE _veda_perm_check (id INT);
  ALTER TABLE _veda_perm_check ADD COLUMN c VARCHAR(8), ADD INDEX idx_c (c);
  UPDATE _veda_perm_check SET c='x' WHERE id=0;
  DROP TABLE _veda_perm_check;          -- 四条全过 = migrate 权限够
  ```
  (本次 0.1.15 只新增 `veda_file_blobs` 一张 `CREATE TABLE`,无新 ALTER。)
- **OTLP 已开**(2026-07-30 补配)。此前长期是关的,根因是一条错误结论——"这台没装 monitor-agent 所以推不了"。
  **veda 的 OTLP 不走本地 agent**:`obs/otlp/discovery.rs` 向 `monitor` 配置服务 GET collector 列表,
  再 gRPC 直推远端 `10.79.11.x:5318`,本地有没有 agent 无关(三台节点本地 5317/5318 都无 listener)。
  实测:`.85` curl discovery 返 200 + 12 个 metrics collector(与测试节点同一批),5318 TCP 全 OPEN。
  **新节点/换库时 `[otlp]` 属于必配段**,漏了不会报错——只会静默无指标(启动日志唯一线索是
  `OTLP metrics exporter disabled`)。`.85` 现配(值取自本机 `/etc/ddmc/env.yaml`,root 才能读):
  ```toml
  [otlp]
  enabled = true
  appname = "veda-reach"
  env_name = "hw-pe1"        # 生产;.89 测试是 hw-tes
  monitor = "paasconf-hw-sh.ddmc-inc.com"
  ```
  ⚠️ **`.85` 生产与 `.89` 测试的 appname 同为 `veda-reach`**,平台上按 appname 查会把两个环境混在一起,
  必须用 `env_name`/`env_level` label 区分。别为了区分改 appname——平台只认注册过的 appname。
- **.85 上没有 mysql 客户端**(mysql/mariadb/mycli/docker 全无,pip 不能联网):MySQL 操作(权限预检/outbox 查询)走
  Mac 本地 mysql client + SSH 隧道:`ssh -f -N -L 13385:<mysql-host>:3306 10.79.55.85`。凭证从节点 config.toml 的
  DSN 取(密码是 percent-encoded,记得 decode),写进 600 权限临时文件用 `--defaults-extra-file` 喂,用完即删。

---

## 发布后观察期(生产必走)

swap + 阶段 3 即时验证通过后,**盯 10–15 分钟**再宣布完成——内存泄漏 / outbox 堆积 / 慢查询这类即时探活看不出:
```bash
ssh -o ServerAliveInterval=20 10.79.55.85 'journalctl -u veda-server --since "10 min ago" --no-pager | grep -iE "error|panic|dead" | tail; echo "RSS: $(ps -o rss= -C veda-server | tr -d " ")KB"'
# MySQL: SELECT status,COUNT(*) FROM veda_outbox GROUP BY status;   -- dead 不应增长
```
盯:无新 error/panic、outbox `dead` 不增长、RSS 平稳、错误率/p99 正常。任一异常 → 按上方回滚(先过数据兼容 gate)。

---

## 附录:2026-06-24 0.1.15 发布踩坑实录(为什么有上面的铁律)

1. **孤儿 build 拖垮 `.161`**:首次用 `ssh '.161' 'cargo build'` 前台跑,Mac 端 SSH 超时断开,但 `.161` 服务端没察觉,cargo 继续编、一堆 rustc 吃满 4 核 → sshd banner-exchange timeout(连不上)+ 公网入口 502。Joe `pkill rustc` 后即恢复。→ **铁律 1**。
2. **rate-limit**:短时间多次串行 SSH `.161`,sshd 限速。→ **铁律 2**。
3. **remote_e2e 误判**:`.161` 被孤儿 build 拖垮期间,remote_e2e 打公网入口全 502(24 failed);`.161` 恢复后重跑 33 passed 全绿——失败是基础设施不是代码。→ 验证要分清「测代码」vs「测部署 endpoint」。
4. **glibc 兼容**:`.89`/`.85` 是 2.34,binary 可复用且向后兼容 `.161` 的 2.38;反之不行。→ **铁律 3**(统一在 2.34 build)。
