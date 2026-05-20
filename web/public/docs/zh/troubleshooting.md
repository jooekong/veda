# 常见问题

## `veda` 报 "unauthorized"

账号 key 或 workspace key 被撤了 / 过期了。两种处理：

- 回 [首页](/) 重新匿名注册，把新 `vk_` 用 `veda init --import-key vk_…` 导入。
- 或者在 [Console](#/console) 给现有 workspace 重新生成一把 key，再导入。

配置在 `~/.config/veda/config.toml`。直接编辑里面的 `api_key` 也行。

## `veda` 抱怨 server URL

确认 `~/.config/veda/config.toml` 的 `server_url` 对。或者命令级覆盖：

```bash
veda --server https://veda.dbpaas.dingdongxiaoqu.com ls
```

## 搜索返回空

- 嵌入是**异步**的——上传后等几秒；如果没命中，过 5s 再试一次。
- 检查 server 是否可达：`veda status`。
- 文件特别小（几个词）BM25 可能不命中。试 `--mode semantic`。
- 字面匹配确保命中：`veda grep "字符串"`（同步，无 embedding 延迟）。

## FUSE 挂载显示旧数据

SSE 事件流可能断了。Mount 会自动重连；等不及的话 `fusermount -u ~/veda && veda-fuse mount …` 彻底重置。

## 匿名账号"丢了"

账号本身还在服务器上，但匿名注册时拿到的 `vk_` **只在那台浏览器**。如果清了 localStorage 或换了浏览器，没 claim 过的话恢复不了。

**怎么避免**：在 [Console](#/console) 页点 **Claim account**，加邮箱密码。之后任何机器上 `veda init --login --email …` 都能登回来。

## 写操作报 "permission denied"

你的 key 是只读的。在 [Console](#/console) 重新签一把 `readwrite` key。

## 发现 bug

去 [git.ddxq.mobi/middleware/dbpaas/veda](http://git.ddxq.mobi/middleware/dbpaas/veda) 提 issue，带上 `veda --version` 输出和你跑的完整命令。
