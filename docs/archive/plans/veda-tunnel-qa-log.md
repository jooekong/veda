# tunnel 质量遥测闭环（T1）：问答日志 + 点赞点踩 + console 统计

> 来源：`docs/design/tunnel-directions.md` T1（第一波 ★）。
> 目标一句话：每次企微问答自动落日志、用户点赞点踩回流、console 能看统计和 bad case 清单——替掉 DAL 真题的人肉收集，产出「知识库缺什么文档」的清单。
> 状态：**方案已定稿**（2026-07-13 三个开放问题经 Joe 拍板，见 §7）。

---

## 1. 全景

```
企微用户 @bot 提问
  └─ tunnel handler：回答（answer/search/错误话术）
       ├─ stream 首帧带 feedback.id=<uuid>（激活企微点赞点踩 UI）
       └─ 回答结束 → INSERT veda_tunnel_qa_log（best-effort，失败仅 warn）
企微用户 点赞/点踩
  └─ feedback_event → 按 feedback.id 关联 → INSERT veda_tunnel_qa_feedback
console #/admin/tunnel
  └─ tunnel admin API：/admin/stats + /admin/qa-log → 统计卡片 + bad case 列表
```

进程间零新依赖：表在生产 veda 库（与 bots 表同库），tunnel 独占读写；veda-server 不参与（平台面 v2 再议）。

## 2. 数据模型（tunnel bootstrap 建表，owner=tunnel）

```sql
CREATE TABLE IF NOT EXISTS veda_tunnel_qa_log (
    id             BIGINT AUTO_INCREMENT PRIMARY KEY,
    ts             TIMESTAMP    NOT NULL DEFAULT CURRENT_TIMESTAMP,
    bot_id         VARCHAR(128) NOT NULL,
    chat_type      VARCHAR(16)  NOT NULL,        -- single | group
    chat_key       VARCHAR(191) NOT NULL,        -- 群=chatid，单聊=userid（复用作 T6 reachable 会话表）
    user_id        VARCHAR(128) NOT NULL,        -- 提问者企微 userid
    query          TEXT         NOT NULL,
    outcome        VARCHAR(16)  NOT NULL,        -- 见 §3 枚举
    hit_count      INT          NOT NULL DEFAULT 0,
    citation_count INT          NOT NULL DEFAULT 0,
    latency_ms     INT          NOT NULL DEFAULT 0,
    answer_text    MEDIUMTEXT   NULL,            -- 开放问题 Q1：存不存原文
    feedback_id    VARCHAR(64)  NULL,            -- 发往企微的 feedback.id（uuid）
    KEY idx_bot_ts (bot_id, ts),
    KEY idx_outcome (outcome, ts),
    KEY idx_feedback (feedback_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

-- 群聊里多人可对同一条回答分别点赞/踩 → 一对多独立表
CREATE TABLE IF NOT EXISTS veda_tunnel_qa_feedback (
    id          BIGINT AUTO_INCREMENT PRIMARY KEY,
    ts          TIMESTAMP    NOT NULL DEFAULT CURRENT_TIMESTAMP,
    feedback_id VARCHAR(64)  NOT NULL,           -- 关联 qa_log.feedback_id
    user_id     VARCHAR(128) NOT NULL,
    kind        TINYINT      NOT NULL,           -- 协议枚举：赞/踩/取消（实现时按 101027 实测定值）
    reason      TINYINT      NULL,               -- 踩的不准确原因 1-4（协议枚举）
    KEY idx_feedback (feedback_id),
    UNIQUE KEY uk_fb_user (feedback_id, user_id) -- 同人改评价 = REPLACE 语义
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
```

写入时序：回答开始生成 `feedback_id`（uuid，随 stream 首帧发出）→ 回答**结束后一次性 INSERT** qa_log（含最终 outcome/latency，避免两段写）→ feedback_event 到达时按 feedback_id upsert feedback 表（找不到对应 qa_log 行也照存——日志丢失不丢反馈）。

## 3. outcome 枚举（对齐 handler 现有分支）

| outcome | 场景 |
|---|---|
| `answered` | answer 正常返回（含引用） |
| `no_context` | 检索空 → 固定话术「知识库中没有找到相关内容」——**这个清单 = 内容缺口** |
| `ungrounded` | answer 返回但零有效引用（server 端 2026-07-16 起零引用返回空 citations，此分支才真正可达） |
| `raw_search` | `[answer] enabled=false` 时的纯检索直出 |
| `error` | 5xx/超时（附现有错误话术分支：502/504） |
| `throttled` / `disabled` | 429 / 501 |

## 4. 改动面

| 处 | 内容 |
|---|---|
| `veda-tunnel/store.rs`（或新 `qa_log.rs`） | 两表 bootstrap（沿用 information_schema 迁移模式）+ `log_qa()` / `upsert_feedback()` / 统计查询（按天聚合 + outcome 分布 + bad case 分页） |
| `wecom/handler.rs` | 回答路径埋点（起止时间、outcome 映射、feedback_id 生成进首帧） |
| `wecom/protocol.rs` + `conn.rs` | `feedback_event` 帧解析分支（**实现前置：101027 的事件 payload 字段实测确认**——调研只证实事件存在，未细化字段名） |
| `admin.rs` | `GET /admin/stats?days=7`（总量/outcome 分布/踩率，按 bot 可选过滤）+ `GET /admin/qa-log?outcome=&feedback=&page=`（bad case 列表） |
| `web/src/admin.ts` | tunnel 页加「问答统计」区块：数字卡片（7 日答题量 / no_context 率 / 踩率）+ bad case 表格（时间/问题/结果/反馈），不引图表库 |

不做（v1 明确砍掉）：平台 API 暴露统计（AI 工作台侧 v2 再议）、报表导出、按用户维度分析、答案重新生成按钮。

## 5. 隐私与保留

- query / answer 落库对象 = 知识库管理员可见（admin console，生产 token）——可见性与知识库内容本身一致，不升级敏感面；文档（deploy-tunnel.md）明示「问答日志含提问原文」
- user_id 为企微 userid（内部员工标识），不脱敏（bad case 回访需要）；开放问题 Q2 定保留策略

## 6. DoD

1. 真机企微问一题 → qa_log 出现一行，outcome/hit_count/citation_count/latency 正确
2. 真机点赞 + 点踩（带原因）→ feedback 表各一行，kind/reason 正确；同人改评价不产生重复行
3. no_context 问题（问知识库没有的内容）→ outcome=`no_context`
4. console 统计区渲染真实数据；bad case 列表可按 outcome/feedback 过滤
5. qa_log INSERT 人为致败（改表名试注入失败路径）→ 回答主流程不受影响，仅 warn
6. 单测：outcome 映射、统计 SQL 聚合逻辑；集成测试走真 MySQL（veda_it）

## 7. 拍板记录（2026-07-13，Joe）

- **Q1 答案原文**：**存**（`answer_text` MEDIUMTEXT）。归因一屏看完；量级每年 <1GB；可见性=admin console，与知识库内容一致不升级敏感面。
- **Q2 保留策略**：**v1 不自动清理**。人工清理 SQL 写进 deploy-tunnel.md，需要时再加 TTL。
- **Q3 console 形态**：**数字卡片 + 可过滤列表，无图表库**。趋势图等有真实需要再上。

## 8. 实现顺序

1. tunnel：qa_log 存储模块（两表 bootstrap + 写入/查询）+ 单测
2. tunnel：handler 埋点（outcome 映射 + feedback_id 进 stream 首帧）——**前置：实测 `feedback_event` 真实 payload**（101027 只证实存在，字段名要抓真帧确认；用真 bot 点一次赞取 journal）
3. tunnel：feedback_event 解析分支 + upsert
4. tunnel：admin `/admin/stats` + `/admin/qa-log` 端点 + 集成测试（真 MySQL）
5. web：tunnel 页统计区块（卡片 ×3 + bad case 表格）
6. 部署 .95 + 真机 DoD 全项验收 + runbook/ARCHITECTURE/memory 更新

## 9. 实现与验收记录（2026-07-14）

- **feedback_event 真帧**（.95 真机抓取）：`body.event.feedback_event = {id: <我们的 feedback.id>, type: 1|2, inaccurate_reason_list: [..]}`，投票人在 `body.from.userid`；踩带原因**数组**（表按 plan Q4 存首个）。真帧已固化为 protocol.rs 单测。
- **outcome 判定修正**（真机数据纠偏）：语义检索恒返回 top-k，`hit_count==0` 永不触发——no_context 改为**匹配 server 固定拒答话术前缀**（`NO_CONTEXT_ANSWER` 常量与 veda-core 逐字对齐，注释互指）。「今天天气怎么样」类问题由 ungrounded 正确归入 no_context。
- **验收**：34 单测（含 3 条真帧解析）+ 真 MySQL 集成测试（bootstrap 幂等/改评价替换/孤儿反馈/聚合/三种过滤）全绿；.95 真机四问全部落行（answered 含完整答案原文+6 引用，延迟 2.2-6.8s）；`/admin/stats`、`/admin/qa-log` 经公网 console 链路 200。
- **插曲**：验收期间 bot 曾被 console 页误删致断服 5 分钟（nginx log 定位到浏览器 DELETE），已重建；顺手补了 admin 写路径审计日志（增/删/改各一条 info）——此前为零审计盲区。
- **部署**：.95 生产 + .89 测试实例同版本；console 统计区（三卡片+过滤明细）三入口已刷。
