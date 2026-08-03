# OnePaaS 接口规范对齐方案（veda 平台面）

> 依据：公司「接口开发规范」(confluence pageId=17371672，冯琪 2021-08-17) +
> aidoc 现役孪生服务 rago / hypermnesia 实证。
> 规范只硬性规定 **RESTful URL / 分页 / 错误** 三块；未规定项见 §2「推断」（Joe 2026-06-16 授权）。

## 0. 范围（推断）

只在 **平台面**（OnePaaS 网关 fronting 的 router：`/v1/workspace/{workspace}/*` + `/v1/my/projects` 及经网关暴露的数据面）套 OnePaaS 信封。
（本文写作时该面还叫 `/v1/apps/{app_id}/*`，2026-06-17 已改名成 workspace/project 模型，旧路径无路由。）
veda-core、直连 `vk_`/`wk_` Pinecone API、已发布 Java SDK **不动**——平滑过渡，扩大范围只是给更多 router 挂同一适配层。

## 1. 标准（规范原文要点）

### 1.1 RESTful URL
- 结构 `${域名}/${占用符}/${微服务}/${版本}/${对象}`，分词符 `_`；占用符 `api`(平台内/已登录) vs `openapi`(对外)。
- 方法：List=`GET /对象`，Insert=`POST /对象`，Get/Update/Delete=`…/对象/:id`，Aggregate=`GET /对象[/:id]/视图`，Action=`POST /对象[/:id]/指令`，Subobject=`/对象/:id/子对象[/:id]`。
- **veda 已合规**：`vectors/{upsert,search,query,delete}` = Action API（指令）；`workspaces`/`datasets` = List/Insert/Delete。**无需重构路由**。
- 网关 base（实测 2026-06-16，占用符是 `proxy` 不是 `api`）：
  - prod `https://paas-api.ddmc-inc.com/proxy/veda`
  - test `https://paas-api-test.ddmc-inc.com/proxy/veda`
  - 全路径如 `https://paas-api.ddmc-inc.com/proxy/veda/v1/vectors/upsert`。

### 1.2 分页（扁平，无外层壳）
- 请求参：`page`(从 1) / `size` / `order_by` / `order`(asc|desc)。
- 返回：`{ data:[...], page, size, order_by, order, total, total_page, has_next_page, has_prev_page }`。

### 1.3 错误（HTTP 非 2XX）
```json
{ "error": { "code": "access_denied", "reason": "user_not_found",
             "message": "用户可读信息", "external": { "request_id": "..." } } }
```
`code`=错误类型，`reason`=类型下具体错误，`error`(代码异常详情，建议不打)。

## 2. veda 现状 → 目标映射

### 2.1 成功响应 —— 一律分页信封（Joe 定：没有不分页场景）
**所有**成功响应统一用分页信封，`data` 恒为数组，单对象=单元素数组。无裸对象、无非分页成功。前端永远只面对两种形状：成功 `{data:[...], page,...}` / 失败 `{error:{...}}`。
| 类别 | 现 | 目标 |
|---|---|---|
| 列表 | `{items, has_more, next_cursor}` | `{data:[...], page, size, order_by, order, total, total_page, has_next_page, has_prev_page}` |
| 单对象(create/get) | `{success:true, data:{...}}` | `{data:[{...}], page:1, size:1, total:1, total_page:1, has_next_page:false, has_prev_page:false}` |
| search/query hits | `{hits:[...]}` | `{data:[...hits], page, size, total, ...}` |

### 2.2 错误
| | 现 | 目标 |
|---|---|---|
| 体 | `{"success":false,"error_code":"INVALID_INPUT","error":"text: must not be empty"}` | `{"error":{"code":"INVALID_INPUT","reason":"text","message":"text: must not be empty","external":{"request_id":"<trace>"}}}` |
| HTTP | REST 4xx/5xx | **保留 REST**（Joe 已选） |

映射规则：`error_code`→`error.code`（保留 UPPER_SNAKE 稳定机器码）；原 `error` 文案→`message`；`reason`←具体子因（InvalidInput=字段名，其余默认=code）；`external.request_id`←现有 trace id；**不打** `error.error`（与「内部错误不外泄」一致，见 error.rs）。
204 无体接口维持（无体即无 `error`，前端按 2XX 处理）。

### 2.3 分页
| | 现 | 目标 |
|---|---|---|
| 请求 | `limit` + `after`(游标) | `page` + `size` + `order_by` + `order` |
| 返回 | `{items, has_more, next_cursor}` | `{data, page, size, order_by, order, total, total_page, has_next_page, has_prev_page}` |

**唯一非纯适配**：store 加 offset+count 路径（cursor→offset；`total`/`total_page` 需 count 查询）。
`order_by` 限 `{created_at, id}`，默认 `created_at desc`；`size` 默认 20 / 上限 200。
受影响端点：**全部返回数据的端点**——workspace/dataset/keys 列表、vector `search`/`query` hits、以及单对象 create/get（单元素数组）。
⚠️ **vector `search`/`query` 的分页语义**：page/size 作用在结果集上，`total` = 本次命中数（受 `top_k`/`ids` 约束），**非全库计数**——ANN/按 id 查没有天然全库 total。`search` 的 `top_k` 与 `size` 取并集语义（先 ANN 取 top_k，再按 page/size 切片）待实现时定。

## 3. 实现

- **veda-types** 新增 `onepaas` 模块：
  - `OnePaasError { code, reason, message, external }`
  - `PageResp<T> { data, page, size, order_by, order, total, total_page, has_next_page, has_prev_page }`
  - `PageQuery { page, size, order_by, order }`
- **veda-server** 平台 router 出口加一层 response 映射（axum `IntoResponse` wrapper / middleware）：
  - `Ok(单对象)` → 裸 body
  - `Ok(分页)` → `PageResp`
  - `AppError` → `{error:{...}}`（错误码映射 + trace id）
  - handler 内部仍返现有 `ApiResponse<T>` / 领域对象，**出口翻译，不碰 53 调用点**。
- **error.rs**：加 `fn onepaas_error(&VedaError, trace_id) -> OnePaasError`。
- **store**：加 offset+count 列表方法（仅平台分页端点用）。

## 4. 落地顺序

1. veda-types 加 `onepaas` 类型（无行为，安全）。
2. error.rs 错误映射 + 平台 router 错误出口切 `{error:{}}`。
3. 平台成功出口切裸对象。
4. store offset+count + 平台分页端点切 `PageResp`。
5. 集成测试（真 Milvus/MySQL）覆盖三件套。
6. 文档（task1）：`onepaas-veda-{intro,api}.md` 按目标格式写示例。

## 5. 待确认（不阻塞，可后置）

- ✅ slug=`veda`、网关 base（prod/test）已实测确认（见 §1.1 / §6）。
- ✅ 成功壳已定：一律分页信封、`data` 恒数组（Joe：没有不分页场景）。
- `search`/`query` 的 `top_k`×`page/size` 切片语义，实现时定。
- 取 token 流程（应用 LLM API Token vs 工作空间服务账号）。
- 范围是否扩到直连 API。

## 6. 网关实测（2026-06-16，curl 须 `--noproxy '*'` 绕 Clash 直连）

- prod/test 两套网关均可达；`/proxy/veda` **全路径**（含 `/healthz`、`/v1/ready`）无 token 一律网关层 401：
  ```
  HTTP 401  Content-Type: application/json;charset=UTF-8
  {"error":{"code":"Forbidden","reason":"","message":"无法通过身份认证"}}
  ```
- 含义：① 公司错误信封 `{error:{code,reason,message}}` 是**网关实打实强制**的，方案错误目标正确；② 鉴权在网关（外移），与 veda `apps.rs` 设计一致——业务方持服务账号 token，网关校验后才转发给 veda；③ 网关 `code` 用 `"Forbidden"` 这类可读串，veda 自身错误码可沿用 `UPPER_SNAKE`（仅风格差异，不影响前端按 `error` 判成败）。
