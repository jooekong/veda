# CLI 速查

权威参考是 `veda --help` 和 `veda <子命令> --help`。本页列最常用的。

## 设置

```bash
# 用账号 key (vk_…) 连
veda init --server https://veda.ddmc-inc.com --import-key vk_xxx

# 或用 workspace key (wk_…)
veda init --server https://veda.ddmc-inc.com --import-key wk_xxx

# 直接用 CLI 注册带邮箱的账号
veda init --email you@example.com --password 'strong-pw'

# 已有账号登录（拿一把新的 login key）
veda init --login --email you@example.com

# 当前账号下加一个新 workspace
veda workspace add my-project
```

配置在 `~/.config/veda/config.toml`。`veda config show` 看当前状态。

无 config 直连（CI / 脚本 / agent 场景，零落盘）：

```bash
export VEDA_SERVER=https://veda.ddmc-inc.com
export VEDA_KEY=wk_xxx
veda search "..."               # 数据面命令直接可用，不写任何本地文件
```

优先级 `--server` / `--key` flag > 环境变量 > config.toml（与 `veda-fuse` 同名同序）；`veda status` 会标注凭证来源（env / config）。

## 文件系统

```bash
veda cp ./README.md /docs/readme.md          # 上传：本地 → 远端
veda cp ./src /code                          # 目录递归上传（src 是目录时自动 recursive）
veda cp - /notes/scratch < input.txt         # 从 stdin 上传（src 写 "-"）
veda cat /docs/readme.md > ./readme.md       # 下载：远端 → 本地（用 cat 重定向，cp 只负责上传）
veda mv /old.md /archive/old.md
veda rm /tmp                                 # 删除（目录默认递归，没有 -r 参数；TTY 下有 y/N 确认）
veda mkdir /new-dir                          # 新建目录
veda append /notes/log "entry"               # 追加内容（也支持 "-" 从 stdin）

veda ls
veda ls /docs
veda ls /docs --json                         # 每行一个 JSON，jq 友好
veda cat /docs/readme.md
veda cat /docs/readme.md --range 10:20       # 1-indexed inclusive 行范围
veda cat /docs/readme.md --head 10           # 头 10 行
veda cat /docs/readme.md --tail 5            # 尾 5 行
```

文本与二进制都能 `cp` / `cat`（需 server ≥0.1.15）。PDF / Word 会自动抽取文本入索引可搜，`cat` 默认输出提取文本、`--raw` 拿原始字节；图片 / jar 等其余二进制只存不索引，`cat` 输出原始字节（重定向到文件）。

## 搜索

```bash
veda search "auth 是怎么做的"                       # hybrid（向量 + BM25 + RRF），默认
veda search "exact term" --mode fulltext
veda search "concept" --mode semantic
veda search "auth" --path /docs                    # 限定子树
veda search "auth" --limit 20
veda search "auth" --detail-level abstract         # 命中只返回 L0 摘要
veda grep "TODO(joe)" --limit 200                  # 字面匹配（同步，无 embedding 延迟），返回 file:line
```

## 问答（RAG）

```bash
veda ask "这个系统怎么部署"            # 一站式回答，内联 [n] 引用 + 出处列表
veda ask "……" --path /docs             # 限定检索子树
veda ask "……" --json                   # 原始 JSON（jq .data.citations 可解析）
```

服务端自主检索并生成带引用的答案，可能需要 10-90s。server 未配 LLM 返回 501、同 workspace 并发问答超限返回 429，均有独立退出码，脚本可区分。

## 摘要分层

```bash
veda abstract /docs/readme.md   # L0 一句话
veda overview /docs/readme.md   # L1 ~2k token 概要
```

异步生成；未就绪返回 `Summary not ready yet`（exit 2），等几秒重试，或直接 `veda cat` 拿原文。

## 结构化 collection

schema 用 JSON array 一次传入；`--embed-source` 指定自动嵌入的字段：

```bash
veda collection create articles \
  --schema '[{"name":"title","type":"string","index":true},
             {"name":"content","type":"string"},
             {"name":"category","type":"string","index":true}]' \
  --embed-source content

# 插入是 JSON 数组（不是单个对象）
veda collection insert articles '[
  {"title":"Intro to Rust","content":"...","category":"tech"},
  {"title":"Pasta","content":"...","category":"food"}
]'

veda collection list
veda collection desc articles
veda collection search articles "systems programming" --limit 5

# 要做过滤 / 聚合走 SQL（collection search 不支持 --filter）
veda sql "SELECT title FROM articles WHERE category = 'tech' LIMIT 5"
veda sql "SELECT category, COUNT(*) FROM articles GROUP BY category"
```

## 杂项

```bash
veda status                     # 当前配置 + server 可达性（含凭证来源 env/config）
veda status --index             # 索引进度 {pending, processing, dead}
veda status --index --wait      # 轮询到全部可搜再退出；有永久失败则退出码非 0（可当 CI 门）
veda config show                # 配置详情
veda --version                  # 客户端版本
```
