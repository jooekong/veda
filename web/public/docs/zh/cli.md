# CLI 速查

权威参考是 `veda --help` 和 `veda <子命令> --help`。本页列最常用的。

## 设置

```bash
# 用账号 key (vk_…) 连
veda init --server https://veda.dbpaas.dingdongxiaoqu.com --import-key vk_xxx

# 或用 workspace key (wk_…)
veda init --server https://veda.dbpaas.dingdongxiaoqu.com --import-key wk_xxx

# 直接用 CLI 注册带邮箱的账号
veda init --email you@example.com --password 'strong-pw'

# 已有账号登录（拿一把新的 login key）
veda init --login --email you@example.com

# 当前账号下加一个新 workspace
veda workspace add my-project
```

配置在 `~/.config/veda/config.toml`。`veda config show` 看当前状态。

## 文件系统

```bash
veda cp ./README.md /docs/readme.md          # 上传
veda cp /docs/readme.md ./readme.md          # 下载（第一个参数以 / 开头即视为远端）
veda cp /docs/readme.md /backup/readme       # 服务端复制
veda cp ./src /code                          # 目录递归上传（src 是目录时自动 recursive）
veda cp - /notes/scratch < input.txt         # 从 stdin 上传（src 写 "-"）
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

仅支持 UTF-8 文本，二进制（PDF / 图片）会被客户端拒绝。

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
veda status                     # 当前配置 + server 可达性
veda config show                # 配置详情
veda --version                  # 客户端版本
```
