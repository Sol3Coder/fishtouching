# 小说阅读器 — 实施计划

> **现状（v1.0）**：界面是 **独立的 ratatui TUI**（`crates/novel-tui`），
> 与 Zed **完全解耦**。Zed 整合方案已终止，理由见 §19。
>
> §1–§9 是仍然有效的设计（legado 数据模型、渲染层、分阶段路线图）。
> **§10–§18 是 Zed 时期的历史归档**（语言服务器 / codeLens / 左侧按钮调研），
> 结论已作废，但「扩展没有 UI API」这些查证结果仍然是真的，留着省得重查。
> 对应的代码（`crates/novel-lsp`、`zed-extension`、`.zed/`）**已删除**。

## 1. 目标

把「编辑器 / 终端」当阅读器用：

1. 界面上显示一段 **自动生成的示例 TypeScript 代码**；
2. 小说正文以 **行注释 `//`** 的形式呈现，章节标题用 `/** */`；
3. 支持：章节上下翻、目录跳转、多书源搜索、换源、正文在线抓取、离线批量下载。

参考实现：`reference/legado-E-main`（Android 阅读 App，Kotlin）。
我们**不移植 Android 层**，只复用它的领域模型（书源 JSON schema + 规则语法）与抓取链路顺序。

## 2. 架构：为什么必须「扩展 + 伴生进程」，以及反向控制怎么实现

### 2.1 扩展的能力边界（已验证）

`ExtensionManifest` 实际字段全集（[`extension_manifest.rs:85-123`](https://github.com/zed-industries/zed/blob/main/crates/extension/src/extension_manifest.rs#L85)）：
`themes`、`icon_themes`、`languages`、`grammars`、`language_servers`、`context_servers`(MCP)、
`slash_commands`、`snippets`、`debug_adapters`、`debug_locators`、`language_model_providers`、`lib`，
加三个**权限项** `process:exec` / `download_file` / `npm:install`。

- ❌ **没有 `tasks`、没有 `agent_servers`**。`AgentServerManifestEntry` 类型存在但未挂进 wasm manifest；
  ACP 扩展自 Zed v1.5.0 已弃用，改用 ACP Registry。
- ❌ wasm WIT 的 import 只有 `context-server, dap, github, http-client, platform, process, nodejs`，
  **没有任何 fs 写入 / Worktree / open_document / applyEdit / UI API**。
- ❌ 唯一写盘能力是 `download_file`（只能写扩展自己的 workdir）+ `make_file_executable`。
- ❌ `process::run_command` 是**阻塞式**（等进程退出、只回收 stdout/stderr），**无法 spawn 守护进程**。
- ✅ 好消息：host 提供 `http_client`（`fetch`/`fetch_stream`），**无需 capability**。
  不过我们用不上 —— 抓取放在原生伴生进程里更好（无沙箱、可并发、可写盘）。
- 佐证 issue：[#18043](https://github.com/zed-industries/zed/issues/18043)（缺 buffer 控制 API，仍 open）、
  [#21208](https://github.com/zed-industries/zed/issues/21208)（Webview，open）。

### 2.2 反向控制：LSP 是唯一可行通道（已验证）

| 机制 | 结论 | 证据 |
|---|---|---|
| `window/showDocument` | ✅ **会打开/聚焦 tab**。客户端能力已声明 `show_document: Some(support: true)`；服务端发请求后 → `lsp_store` 转事件 → `handle_lsp_show_document` → `workspace.open_abs_path(path,{focus})`，然后按 `selection` 定位光标 | [`lsp.rs:1147`](https://github.com/zed-industries/zed/blob/main/crates/lsp/src/lsp.rs#L1147)、[`lsp_store.rs:1241`](https://github.com/zed-industries/zed/blob/main/crates/project/src/lsp_store.rs#L1241)、[`editor/src/items.rs:2515`](https://github.com/zed-industries/zed/blob/main/crates/editor/src/items.rs#L2515) |
| `workspace/applyEdit` | ✅ 能改 buffer，但 **URI 必须是 `file:` 且能转成路径** | [`lsp_store.rs:1101`](https://github.com/zed-industries/zed/blob/main/crates/project/src/lsp_store.rs#L1101)、[`:10393`](https://github.com/zed-industries/zed/blob/main/crates/project/src/lsp_store.rs#L10393) |
| 未命名虚拟 buffer | ❌ **不可行**。Zed 没有 `untitled:` scheme（生产代码里搜不到，只出现在 editor 测试里）。**必须落盘** | `to_file_path_ext()` 强制 `file:` |
| codeLens 点击 | ✅ 可点，触发 `workspace/executeCommand`；不能用「编辑器自有命令」 | [`code_lens.rs:648-690`](https://github.com/zed-industries/zed/blob/main/crates/editor/src/code_lens.rs#L648)、[PR #56222](https://github.com/zed-industries/zed/pull/56222) |
| `codeAction`（灯泡）/ `showMessageRequest`（带按钮通知） | ✅ 可用，作为备选交互入口 | 同上 |

**最低版本要求**：`showDocument` + `executeCommand` 由 [PR #63607](https://github.com/zed-industries/zed/pull/63607)（已 merge，closes [#61572](https://github.com/zed-industries/zed/issues/61572)）才补齐。
在此之前 [issue #53122](https://github.com/zed-industries/zed/issues/53122) 明确记录「`showDocument` 本地文件是 no-op，只返回 success:true」。
→ **P0 第一步就是确认本机 Zed 版本高于该 PR**。

### 2.3 替代路径（全部评估过）

| 路径 | 结论 |
|---|---|
| Slash command | ❌ 只能把文本插入 **Agent(Assistant) 面板**的对话上下文，不能开 tab、不能改 buffer |
| Context server (MCP) | ❌ 只给 Agent 提供 Tools/Prompts，**不影响编辑器 UI**；且官方计划弃用扩展式 MCP |
| Custom agent server / ACP | ❌ wasm manifest 无 `agent_servers`；ACP 扩展 v1.5.0 起弃用。外部 ACP 二进制也只在 Agent 面板内工作 |
| Tasks | ❌ 扩展**不能**贡献 task（manifest 无字段，[#45932](https://github.com/zed-industries/zed/issues/45932) 仍 open）。但 `.zed/tasks.json` 由**项目/用户**提供，可用 `$ZED_WORKTREE_ROOT` 等模板变量 → **这就是我们的触发手段** |
| CLI | ✅ `zed <path>` 在已有窗口打开；`-a/--add` 明确加到当前窗口；`-n` 新窗口；**`zed -` 读 stdin 生成临时文件并打开**；支持 `zed://` URL |
| 外部 HTTP/WS 控 UI | ❌ **不存在**。外部控制只有 URL handler 与 CLI（`zed://file/<path>`、`zed://open`、`zed://settings`、`zed://extension/<id>`） |

**同类先例**：未发现「把 Zed 当阅读器」的现成项目。最接近的先例是 claude-code-zed 等 LSP 类扩展
需要 `showDocument` 打开文件/diff —— 这个需求正是 #63607 落地的。

### 2.4 最终架构

```
┌─ Zed 编辑器 ─────────────────────────────────────────────────────┐
│  tab: .novel/reader.novel   ← showDocument 打开/聚焦              │
│                                applyEdit 全文替换（翻章）          │
│                                codeLens 变成可点击按钮           │
└───────┬──────────────────────────────────────────────────────┘
        │ LSP (stdio)
┌───────┴──────────────────────────────────────────────────────┐
│ ① zed-extension (Rust → wasm32-wasip2)                        │
│    贡献: languages(novel) + language_servers(novel-lsp)       │
│    capability: download_file（拉伴生二进制 + chmod +x）        │
│    职责: 只负责装二进制、返回 Command。不抓章节、不写 worktree   │
├───────────────────────────────────────────────────────────────┤
│ ② novel-lsp (Rust 原生 binary, tokio) —— 伴生进程            │
│    · initialize 声明 codeLensProvider + executeCommandProvider │
│    · 生成 reader.novel 的内容（正文 → // 注释）                │
│    · showDocument 打开/聚焦 tab                                │
│    · applyEdit 全文替换（翻章）                                │
│    · codeLens = 上一章/下一章/目录/换源/搜索                     │
│    · executeCommand 响应点击                                    │
│    · 也可被 novel-cli 通过本地端口驱动（调试/无 Zed 场景）        │
├───────────────────────────────────────────────────────────────┤
│ ③ novel-core (纯 Rust crate, 可独立单测)                       │
│    书源模型 · 规则引擎 · HTTP · 搜索调度 · 正文抓取            │
│    · 净化 · SQLite 缓存 · 下载队列                              │
└───────────────────────────────────────────────────────────────┘
```

把抓取全部放在 ③，是为了能脱离 Zed 用 `cargo test` + 离线 HTML 快照验证 —— 这是整个项目
最容易坏、也最值得测的部分。

### 2.5 触发链路（关键：LSP 何时被拉起）

**LSP 不是随时在跑的** —— Zed 只在「worktree 内出现匹配语言的文件 / 打开对应 buffer」时才
`StartLanguageServer`（[`lsp_store.rs:9950`](https://github.com/zed-industries/zed/blob/main/crates/project/src/lsp_store.rs#L9950)，需实测）。
这正好让触发手段变得优雅：

```
用户按快捷键 / 命令面板选 task
  → .zed/tasks.json: novel: 打开阅读器
  → 执行 zed -a "$ZED_WORKTREE_ROOT/.novel/reader.novel"
  → Zed 打开该文件 → 匹配我们的自定义 language → 拉起 novel-lsp
  → novel-lsp 接管：抓章节 + 写文件 + showDocument + codeLens
```

一次性解决「怎么触发」和「LSP 怎么启动」两个问题。

### 2.6 ⚠ 三个必须绕开的坑

1. **路径必须在当前 worktree 内**。`open_abs_path` 默认 `WorkspaceMatching::MatchExact`，
   不在任何已打开 worktree 内的路径**可能开新窗口**（[`workspace.rs:10903`](https://github.com/zed-industries/zed/blob/main/crates/workspace/src/workspace.rs#L10903)）。
   → 章节文件写进 `<worktree>/.novel/`，**不要**写系统临时目录。
2. **不要一章一个文件**。若每章 `chapter-001.ts` 再 `showDocument`，会开一堆 tab。
   → **固定路径 `.novel/reader.novel`**，翻章 = 重写内容 + `applyEdit` 全文替换 + `showDocument(selection)` 定位。
3. **不要用 `.ts` 扩展名**（除非实测没问题）。写 `.ts` 会让 Zed 同时拉起**真正的**
   `typescript-language-server`，它会对我们的「伪 TS」报诊断，而且白白占资源。
   → 用自定义 language `novel` + 自定义扩展名 `.novel`，但 `languages` 里**引用 TypeScript 的
   tree-sitter grammar**（`tree-sitter-typescript`），这样**高亮效果就是 TS**，
   而 LSP 只有我们的 `novel-lsp`。
   如需 `.ts` 后缀，需先实测 TS server 是否干扰；见 §7 R7。

## 3. 目录结构

```
F:/rust/fishtouching/
  reference/legado-E-main/      # 只读参考
  Cargo.toml                    # workspace
  crates/
    novel-core/                 # ③ 引擎（lib）
      src/{source,rule,http,search,toc,content,purify,store,download}.rs
    novel-lsp/                  # ② LSP 伴生进程（bin）
      src/{main,lsp,render,commands,state}.rs
    novel-cli/                  # 调试/无 Zed 使用的命令行（bin）
  zed-extension/                # ① Zed 扩展
    extension.toml              # grammars/languages/language_servers/capabilities
    src/lib.rs
  fixtures/                     # 书源 JSON + 站点 HTML 快照（单测用）
  .zed/tasks.json               # 触发入口：zed -a $ZED_WORKTREE_ROOT/.novel/reader.novel
  .novel/                       # 运行时生成（应加 .gitignore）
    reader.novel                # ← 阅读 tab，固定路径，内容整段重写
    db.sqlite                   # 书源/书架/章节元数据
    cache/<book_id>/<idx>.txt   # 章节正文缓存
```

## 4. 数据模型（照搬 legado，Rust 化）

`BookSource`（serde，**字段名必须保持 camelCase** 才能吃现成书源 JSON）。
字段来源：`data/entities/BookSource.kt:31-100`，已与 Room schema
`app/schemas/io.legado.app.data.AppDatabase/89.json` 逐字段核对。

```rust
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BookSource {
    book_source_url: String,          // 主键 + baseUrl + 去重键（equals/hashCode 只用它）
    book_source_name: String,
    book_source_group: Option<String>,   // 分隔符 [,;，；]
    #[serde(default)] book_source_type: u8,  // 0文本 1音频 2图片 3文件 4视频
    book_url_pattern: Option<String>,    // 详情页正则；命中则把搜索响应当详情页解析
    #[serde(default)] custom_order: i32,
    #[serde(default = "yes")] enabled: bool,
    #[serde(default = "yes")] enabled_explore: bool,
    js_lib: Option<String>,              // JS 片段，或 {"name":"url"}
    enabled_cookie_jar: Option<bool>,    // ⚠ 三态！见下方警告
    concurrent_rate: Option<String>,     // "1000"=间隔1s；"20/60000"=60s内20次
    header: Option<String>,              // JSON 字符串，或 @js: 求值出 JSON
    login_url: Option<String>,
    login_ui: Option<String>,
    login_check_js: Option<String>,
    cover_decode_js: Option<String>,
    book_source_comment: Option<String>,
    variable_comment: Option<String>,    // 只是说明文本，不是变量值！
    #[serde(default)] last_update_time: i64,
    #[serde(default = "180000")] respond_time: i64,
    #[serde(default)] weight: i32,
    explore_url: Option<String>,
    explore_screen: Option<String>,
    search_url: Option<String>,
    rule_explore: Option<ExploreRule>,
    rule_search: Option<SearchRule>,
    rule_book_info: Option<BookInfoRule>,
    rule_toc: Option<TocRule>,
    rule_content: Option<ContentRule>,
    #[serde(default)] event_listener: bool,
    #[serde(default)] custom_button: bool,
}
```

**两条必须记住的坑：**

1. **`BookSource` 没有 `variable` 字段。** 书源级变量运行时存在
   `CacheManager["sourceVariable_<key>"]`（`BaseSource.kt:242-269`）。JSON 里真没有。
   （`variable` 只存在于 `books`/`chapters`/`searchBooks` 三张表。）
2. **`enabledCookieJar` 是三态**：Kotlin 声明默认 `true`，DB 列默认 `0`，而 GSON 在 JSON
   缺省时给 `null`。代码只在 `== true` 时启用 cookie jar（`AnalyzeUrl.kt:112`）——
   **实际默认是不启用**。Rust 侧用 `Option<bool>` 并只在 `Some(true)` 时启用，否则行为不一致。

子规则对象（字段全部 `String?`，全部可选）：

| 对象 | 字段 | 位置 |
|---|---|---|
| `SearchRule` | `checkKeyWord, bookList, name, author, intro, kind, lastChapter, updateTime, bookUrl, coverUrl, wordCount` | `rule/SearchRule.kt:12-25` |
| `ExploreRule` | `bookList, name, author, intro, kind, lastChapter, updateTime, bookUrl, coverUrl, wordCount` | `rule/ExploreRule.kt:12-23` |
| `BookInfoRule` | `init, name, author, intro, kind, lastChapter, updateTime, coverUrl, tocUrl, wordCount, canReName, downloadUrls` | `rule/BookInfoRule.kt:12-25` |
| `TocRule` | `preUpdateJs, chapterList, chapterName, chapterUrl, formatJs, isVolume, isVip, isPay, updateTime, nextTocUrl` | `rule/TocRule.kt:9-20` |
| `ContentRule` | `content, subContent, title, nextContentUrl, webJs, sourceRegex, replaceRegex, imageStyle, imageDecode, payAction, callBackJs` | `rule/ContentRule.kt:12-25` |

> ⚠ **子规则对象要么是对象，要么是 JSON 字符串**。每个子规则都有自定义 `JsonDeserializer`
> 同时接受两种形态（`SearchRule.kt:29-35` 等）。Rust 侧需要 `deserialize_with`：先 `Value`，
> 是 `String` 就再解析一次。
>
> 逻辑上必填的是 `ruleSearch.bookList` / `name` / `bookUrl`（`BookList.kt:89-98,222-226`），
> 但 JSON schema 上无 required 字段。

### 4.1 规则语法全集

**前缀分派**（`AnalyzeRule.kt:594-628`、`AnalyzeByJSoup.kt:512-520`）：

| 前缀 / 特征 | 分析器 | 备注 |
|---|---|---|
| `@@` | Default (CSS) | 显式默认，去前缀 |
| `@css:` | Default | 去前缀后 `isCss=true`，支持 `@attr` 尾缀 |
| `@xpath:` | XPath | 去 7 字符 |
| `@json:` | Json | 去 6 字符 |
| `$.` / `$[` 开头，**或整个 content 本身是 JSON** | Json | 自动识别（`:614-617`） |
| `/` 开头 | XPath | 自动识别 |
| `:` 开头 | Regex | **仅** `getElement(s)`（allInOne）生效（`:506-512`） |
| `@js:` / `<js>…</js>` | Js | `@js:` **吃满到字符串末尾** |
| `@webjs:` | WebJs | |
| 其余 | Default | JSoup CSS 选择器 |

**`@` 管线**：`class.item.0@tag.a@text` —— 按 `@` 切分，中间段是选择器，**最后一段是取值器**
（`AnalyzeByJSoup.kt:224-315`）：

| 取值器 | 语义 |
|---|---|
| `text` | 文本 |
| `textNodes` | 直接文本节点 |
| `ownText` | 自身文本（不含子元素） |
| `html` | 内层 HTML |
| `all` | 全部 |
| 其它任意字符串 | **当作属性名**（如 `@href`、`@src`） |

**索引与排除**（`AnalyzeByJSoup.kt:238-503`）：

| 写法 | 语义 |
|---|---|
| `tag.div.0` / `class.item.-1` | 老写法，`.` 选择，索引可负 |
| `tag.div.-1:10:2` | 老写法区间 `start:end:step` |
| `tag.div[-1, 3:-2:-10, 2]` | 新写法，方括号内多个索引/区间 |
| `tag.div!0:3` | **`!` = 排除** |
| `tag.div[-1:0]` | 反转 |
| `children` / `class.X` / `tag.X` / `id.X` / `text.X` | 前置规则（不是 CSS） |

> **排除用的是 `!` 或负索引，不是 `-` 前缀。** `-` 前缀只属于列表规则（见 §4.3）。

**链式操作符**：

| 语法 | 语义 | 证据 |
|---|---|---|
| `@put:{json}` | 求值**前**把 JSON 键值存为变量，正则 `@put:(\{[^}]+?\})` | `AnalyzeRule.kt:722,383-411` |
| `@get:{key}` | 插入变量值；查找优先级 chapter → book → ruleData → source | `AnalyzeRule.kt:722,650-652,734-766` |
| `{{ … }}` | `evalPattern = @get:\{[^}]+?\}\|\{\{[\w\W]*?\}\}`（`:723`）。内容 `isRule()`（`@`/`$.`/`$[`/`//` 开头）→ **当规则**对当前 result 求值；否则 → **当 JS 执行**。所以 `{{@@name}}` = 对当前结果套默认规则 `name` | `AnalyzeRule.kt:660-676,706-710` |
| `rule##regex##repl##` | 1 段不算；2 段=只替换；3 段=替换串；4 段=仅取第一个匹配 | `AnalyzeRule.kt:677-703,430-456` |
| `$1`…`$99` | 从**上一个**正则/列表结果里取分组，正则 `\$\d{1,2}` | `AnalyzeRule.kt:724,645-663` |
| `a&&b` | 结果**依次拼接**（concat） | 见下 |
| `a\|\|b` | 取第一个非空后 break | `AnalyzeByJSoup.kt:117-119` |
| `a%%b` | 逐项**交错**合并（zip），按首个结果长度 | `AnalyzeByJSoup.kt:126-131`、`AnalyzeByXPath.kt:74-81`、`AnalyzeByJSonPath.kt:107-114` |

> ⚠ **`&&` / `||` / `%%` 的切分必须在选择器括号与引号平衡组之外**（`RuleAnalyzer.kt:161-237`）。
> 直接 `str::split("&&")` 会把 `[attr="a&&b"]` 切碎。这是最容易写错的一处。
>
> ⚠ **`%%` 只存在于 `getStringList`/`getElements`/`getList`**；标量 `getString` 只支持 `&&`/`||`。

**多值拼接与后处理**：

| 项 | 规则 | 证据 |
|---|---|---|
| `getString` 遇多结果 | 用 **`\n`** 连接 | `AnalyzeByJSoup.kt:53-61`、`AnalyzeByXPath.kt:137-138`、`AnalyzeByJSonPath.kt:44-50` |
| `kind` | `getStringList().joinToString(",")` | `BookList.kt:230`、`BookInfo.kt:85-90` |
| `name` 清洗 | 去 `\s+作\s*者.*\|\s+\S+\s+著` | `BookHelp.kt:477-490`、`AppPattern.kt:32-33` |
| `author` 清洗 | 去 `^作\s*者[:：\s]+\|\s+著` | 同上 |
| `wordCount` | `wordCountFormat()`：>10000 显示「万字」 | `StringUtils.kt:254-271` |

**求值引擎分派流程**（`AnalyzeRule.kt:186-380`）：

1. `splitSourceRuleCacheString` → `splitSourceRule(ruleStr, allInOne)`（`:497-560`）：先用 `JS_PATTERN`/`WebJS_PATTERN`
   扫描切成 `Mode::Js`/`Mode::WebJs` 段，其余段用当前基准 Mode（`:` 开头→Regex，否则 Default）。
2. 每段构造 `SourceRule`（`:594-710`）：前缀定 Mode → `splitPutRule` 摘 `@put:` →
   `evalPattern` 拆 `{{…}}`/`@get:` → `splitRegex` 拆 `$n` → `makeUpRule` 替换占位符 →
   最后按 `##` 拆出 `replaceRegex/replacement/replaceFirst`。
3. 链式执行：`result` 初始 = content，逐条 SourceRule 分派到 JsonPath / XPath / JSoup / Rhino / WebView / Regex（`:228-260,327-360`）。
4. 每轮后若有 `replaceRegex` 则替换（`:430-456`）。
5. `isUrl=true` 时用 `redirectUrl` 绝对化；**结果空则返回 `baseUrl`**（`:262-276`）；`getString` 默认 `unescapeHtml4`。
6. 特例：`result` 是 JS `NativeObject` 时，首个 SourceRule 的 rule 直接当**键名**；`LinkedTreeMap` 同理（`:236-260,308-313`）。

`RuleData`（`RuleData.kt:5-28`）**只携带 `variableMap`**（>10000 字符走 `putBigVariable`）。
`baseUrl` / `redirectUrl` 是 `AnalyzeRule` 自身字段（`setBaseUrl:107-113`、`setRedirectUrl:115-127`）。
**`sourceRegex` 不在 `RuleData` 里**——它是 `ContentRule` 的字段，只传给 `BackstageWebView`。

### 4.2 URL 模板与请求选项（已确认）

URL 不是纯模板，而是 **`url` + 可选 `,{json option}`** 两段，由 `AnalyzeUrl` 按正则
`\s*,\s*(?=\{)` 切分（`model/analyzeRule/AnalyzeUrl.kt:684`）——**注意只在 `,{` 前有空白时才切**：

```
https://site.com/search?q={{key}}&page={{page}}, {"method":"POST","body":"...","charset":"gbk","retry":1}
```

处理顺序（`AnalyzeUrl.kt:139-262`）：
1. 先执行 `@js:` / `<js></js>` 段；普通文本段中的 **`@result` 替换为已有结果**（`:160-186`）；
2. `replaceKeyPageJs`：`<a,b,c>` 按页码取第 n 段，**越界取最后一个**（`:196-207,690`）；
3. `NetworkUtils.getAbsoluteURL(baseUrl, urlNoOption)` 相对→绝对（`:216`）；`baseUrl` 每次解析后更新为 origin（`NetworkUtils.kt:192-203`）；
   `getAbsoluteURL` **跳过 `javascript:` 与 data URL，返回 `""`**（`NetworkUtils.kt:175-190`）；
4. Option 字段：`method, charset, headers, body, type, origin, retry, webView, webJs, dnsIp, js, bodyJs, serverID, webViewDelayTime`（`:714-800`）。
   `headers` 支持 `proxy`；`body` 支持对象或字符串；`js` 求值结果**覆盖 url**；`bodyJs` 二次处理响应体。

> ⚠ **`{{ }}` 在 `AnalyzeUrl` 里一律当 JS 表达式**（`:187-207`），与 `AnalyzeRule` 的
> 「是规则就当规则」语义**不同**。绑定变量：`baseUrl, cookie, cache, page, key, speakText,
> speakSpeed, book, source, result, infoMap, java`（`:326-338`）。所以 `{{@@x}}` 在 URL 里
> **不是规则**。这是端到端测试必须覆盖的易错点。

编码：`encodeParams` 按 `charset` 决定是否 URLEncode；`charset="escape"` → JS `escape`（`:250-300`）。
旧写法 `searchKey` / `searchPage` 仍常见，导入时会转换（`help/storage/ImportOldData.kt:322-323,345-348`）。

超时（`help/http/HttpHelper.kt:59-63`）：connect 15s / read 60s / call 60s。
`retry`（默认 0）只对**非 2xx 立即重发**，无退避（`help/http/OkHttpUtils.kt:29-43`）。

### 4.3 列表规则的前后缀（已确认）

凡是「列表型」规则（`bookList`、`chapterList`）都支持两个前缀（`BookList.kt:88-97`、
`BookChapterList.kt:54-60`）：

| 前缀 | 语义 |
|---|---|
| `-` | 源已倒序，**不要**再 reverse |
| `+` | 仅剥离前缀，保持原序 |
| 无 | 默认 `reverse()`（因为多数站点目录是倒序的） |

> `contentRule.content` **没有** `-` 语法；带前导 `-`/`+` 的只有列表规则。

### 4.4 已确认的链路契约

| 环节 | 契约 | 证据 |
|---|---|---|
| 目录翻页 | `nextTocUrl` 解析成 URL **列表**：0 条→结束；1 条→串行跟随；>1 条→并发抓且**不再取下一页** | `BookChapterList.kt:72,94-111,175-195` |
| 目录防死循环 | **仅**靠已访问 URL 集合（`!nextUrlList.contains(nextUrl)`），**无最大页数上限** | `BookChapterList.kt:72` |
| 目录去重 | `LinkedHashSet` 按 `chapterUrl` 去重，再 `forEachIndexed` 重编序号 | `BookChapterList.kt:129-137` |
| 目录倒序 | 受书级 `book.getReverseToc()` 二次控制 | `BookChapterList.kt:129-137` |
| `updateTime` | **不解析成时间戳**，原样存 `tag`（卷名时 `tag=info`） | `BookChapterList.kt:236-282` |
| `isVip`/`isPay` | `getString(rule).isTrue()` | `BookChapterList.kt:236-282` |
| 正文分页 | `nextContentUrl` 同列表契约；循环内遇到「下一章链接」直接 break | `BookContent.kt:73-102` |
| 正文多值拼接 | JSoup 多元素以 **`\n`** 连接 | `AnalyzeByJSoup.kt:48-60` |
| 正文内置净化 | `HtmlFormatter.formatKeepImg`：`&nbsp;`→空格、块级标签→换行、去注释/标签、`\s*\n+\s*`→`\n　　`、首尾 trim | `utils/HtmlFormatter.kt:24-71` |
| 空正文 | 抛 `ContentEmptyException`（非卷名章节） | `BookContent.kt:204-206` |
| `replaceRegex` | 规则**全文替换**，先逐行 trim，再 `getString(replaceRegex, contentStr)` | `BookContent.kt:168-175` |

### 4.5 净化管线（两段式，已确认）

**抓取时**：`HtmlFormatter.formatKeepImg` →（可选）`contentRule.replaceRegex` → `unescapeHtml4`。

**渲染时**（`help/book/ContentProcessor.kt:89-221`，顺序固定）：
1. 去重复标题（正文开头 `^(\s|\p{P}|书名)*标题\s*` 截掉）；
2. `reSegment` 重新分段（**中文引号启发式，桌面端直接砍掉**）；
3. 简繁转换（**砍掉**）；
4. `<usehtml>…</usehtml>` 用占位符保护；
5. **替换规则**：逐行 trim → 顺序应用，`isRegex` 走 **3000ms 超时**的 regex replace，超时则**自动禁用该规则**；
6. `includeTitle` 决定是否把标题加回；
7. 分段输出：按 `\n` split，trim 掉 `code<=0x20` 与 `　`，空段丢弃，首段不缩进、其余加缩进前缀。

> 我们要保留 1、5、6、7；砍掉 2、3、4。

## 5. 渲染层：正文怎么变成 TS

文件路径固定为 **`<worktree>/.novel/reader.novel`**（理由见 §2.6），内容渲染成
「看起来合理的 TS 文件」：

```ts
/**
 * 《书名》 · 第 12 / 345 章 · 源: xxx
 *
 *   ▶ 上一章    ▶ 下一章    ▶ 目录    ▶ 换源     ← 由 codeLens 提供可点击按钮
 */

// ── 第12章 夜色渐浓 ──────────────────────────────
// 他推开门，风灌了进来，吹得桌上的稿纸哗哗作响。
// “你终于来了。”那人背对着他说。
// ……（正文按行加 `// ` 前缀）

export interface ChapterMeta { book: string; index: number; total: number }

export const chapter: ChapterMeta = { book: "书名", index: 12, total: 345 };

// ── 章节末尾 ─────────────────────────────────────
```

要点：
- **每行 `// ` 前缀**（而不是一个 `/* */` 大块）——软换行、搜索、光标移动都正常，
  且 ZED 的注释折叠/选中行为符合直觉。
- **内容永远是语法合法的 TS**（正文全在行注释里），所以不会污染语法树。
- 正文里混入合法 TS 声明，避免整页只有注释（用户要求「示例用 ts 代码」）。
- 翻章 = 重新生成整段文本 + `workspace/applyEdit` 全文替换（章节 5~15KB，量级完全够用）
  + 写盘（保持磁盘与 buffer 一致）→ `showDocument(selection)` 把光标定位到章首。
- ❌ **不能**用无磁盘虚拟 buffer：已验证 Zed 没有 `untitled:` scheme（§2.2），必须落盘。
- ⚠ 用 `.novel` 扩展名 + **TypeScript 的 tree-sitter grammar**（`tree-sitter-typescript`）：
  高亮是 TS 的，但不会拉起真正的 `typescript-language-server`。
  若坚持 `.ts` 后缀，先实测 TS server 是否干扰（§7 R7）。

### 5.1 交互入口：codeLens

在文件顶部与末尾各发一组 codeLens（`textDocument/codeLens`）：

| codeLens 标题 | executeCommand | 语义 |
|---|---|---|
| `▶ 上一章` | `novel.prev` | 重渲染 + applyEdit |
| `▶ 下一章` | `novel.next` | 同上 |
| `📑 目录 (12/345)` | `novel.toc` | 换成目录视图（同一 tab） |
| `🔁 换源` | `novel.switchSource` | 弹 `showMessageRequest` 选源 |
| `🔍 搜索` | `novel.search` | 换成搜索输入/结果视图 |

每条 codeLens 可带 `command.arguments`，把 book/chapter index 传回去，服务端不用查状态。

> 备选入口：`codeAction`（灯泡）、`showMessageRequest` 带按钮的通知。
> 如果 codeLens 体验不佳（需手动 toggle、无键盘动作），降级到这两个。

## 6. 分阶段计划

### P0 — 可行性 spike（**先做这个，其余都可能是幻觉**）

目标：证明「task 触发 → 自定义语言 → LSP 被拉起 → 打开并更新一个 tab」整条链路成立。
**不碰 legado。** 这一步的结论直接决定 §2 架构是否成立。

**P0.0 前置检查（半小时，先做）**
- [ ] 确认本机 Zed 版本含 [PR #63607](https://github.com/zed-industries/zed/pull/63607)。
      该 PR 之前 `showDocument` 本地文件是 **no-op**（[#53122](https://github.com/zed-industries/zed/issues/53122)）。
      版本不够就在 P0.3 前停住，直接走降级方案。

**P0.1 骨架**
- [ ] 建 cargo workspace + `zed-extension`
- [ ] `extension.toml`：声明 `[grammars.typescript]`（指向 `tree-sitter/tree-sitter-typescript`）、
      `[languages.novel]`（`extensions: ["novel"]`，`grammars: ["typescript"]`）、
      `[language_servers.novel-lsp]`；`capabilities = ["download_file"]`
- [ ] `novel-lsp` 实现最小 LSP handshake（`initialize` / `initialized` / `shutdown` / `exit`），
      日志写到文件（LSP 的 stderr 在 Zed 里不好看）
- [ ] 在项目根建 `.zed/tasks.json`，一条 task：`zed -a "$ZED_WORKTREE_ROOT/.novel/reader.novel"`

**P0.2 打通“LSP 被拉起”**
- [ ] 手建 `.novel/reader.novel`，用 task 打开它
- [ ] 确认 Zed 匹配到 `novel` language 并 **真的拉起了 `novel-lsp`**
      （若未拉起：尝试用 `language_servers` 的其他触发时机，或改为在 task 里直接启动伴生二进制）

**P0.3 打通反向控制（核心验证）**
- [ ] `initialize` 声明 `codeLensProvider` + `executeCommandProvider`
- [ ] 写硬编码 TS 示例内容 → `window/showDocument` 打开 tab（验证 **不会** 开新窗口，§2.6 坑 1）
- [ ] 发 `textDocument/codeLens`，出现「上一章 / 下一章」按钮
- [ ] 点击 → 收到 `workspace/executeCommand` → 重生成内容 → `workspace/applyEdit` 全文替换
      → `showDocument(takeFocus: true, selection: line 0)` 定位

- **验收**：手点 codeLens，tab 里内容从「第一章」变成「第二章」，**且只有一个 tab**。
- **失败降级**（按顺序尝试）：
  1. `applyEdit` 不生效 → 直接重写磁盘文件 + `showDocument`（靠 Zed 的 fs watcher 刷新）
  2. codeLens 不好用 → 改用 `codeAction`（灯泡）或 `showMessageRequest`（带按钮通知）
  3. `showDocument` 完全不可用 → 完全放弃 LSP 反控，改为「`novel open` 写文件 + `zed -a <file>` CLI」，
     翻章 = 重跑 task（tab 内容刷新）

### P1 — 书源引擎 MVP（`novel-core`）

**反序列化**
- [ ] `BookSource` serde 模型（§4 全字段）+ 去重（按 `bookSourceUrl`，DB 唯一索引 + `on_conflict=REPLACE`）
- [ ] 子规则对象的**双形态**兼容：对象 **或** JSON 字符串（`deserialize_with` 先 `Value` 再重解析）
- [ ] `enabledCookieJar` 用 `Option<bool>`，只在 `Some(true)` 时启用（对齐 legado 实际行为）

**规则拆解器**（先写单测，这是引擎的地基）
- [ ] `split_source_rule`：先扫 `@js:` / `<js>…</js>` / `@webjs:` 段（`@js:` **吃满到末尾**），其余段定 Mode
- [ ] Mode 分派：`@@` / `@css:` / `@xpath:` / `@json:` / `$.`|`$[`|纯JSON→Json / `/`→XPath / 默认 CSS
- [ ] 平衡组切分器：`&&` / `||` / `%%` **只在方括号与引号之外**切（`RuleAnalyzer.kt:161-237`）
- [ ] `@put:{json}` 摘除（求值前）→ `@get:{key}` 插值 → `{{…}}` 求值 → `$n` 捕获组 → `##` 拆分（2/3/4 段语义）
- [ ] `{{ }}` 双语义：内容是规则（`@`/`$.`/`$[`/`//` 开头）→ 当规则；否则当 JS（MVP：JS 分支直接报「不支持」）

**取值器**
- [ ] CSS（用 `scraper`）：`@` 管线，末段为取值器 `text`/`textNodes`/`ownText`/`html`/`all`/任意属性名
- [ ] 索引与排除：`tag.div.0`、`.x.-1`、`[-1, 3:-2:-10, 2]`、`!0:3`、`[-1:0]`、前置 `children`/`class.X`/`tag.X`/`id.X`/`text.X`
- [ ] Json：`serde_json` + `$.a.b[0]` 子集（完整 JsonPath 可推后）
- [ ] 多值：`get_string` 用 `\n` 拼接；`kind` 用 `,` 拼接
- [ ] 后处理：`formatBookName` / `formatBookAuthor` 正则清洗、`wordCountFormat`

**URL 与请求**
- [ ] 按 `\s*,\s*(?=\{)` 切 URL 与 option（**仅 `,{` 前有空白才切**）
- [ ] `<a,b,c>` 页码取段（越界取最后一个）、相对→绝对（跳过 `javascript:`/data URL）
- [ ] option：`method` / `headers` / `body` / `charset` / `retry`（其余字段先忽略但**不要报错**）
- [ ] `{{ }}` **在 URL 里一律当 JS** —— MVP：只支持内置变量 `page`/`key`/`baseUrl`/`book.name` 等白名单，
      碰到无法求值的表达式要**显式报错**，绝不静默为空
- [ ] 超时：connect 15s / read 60s / call 60s；`retry` 仅对非 2xx 立即重发

**链路**
- [ ] 列表规则前缀 `-` / `+` / 无（默认 reverse）—— 抽成共享函数，`bookList` 与 `chapterList` 复用
- [ ] 目录抓取：`ruleToc` + `nextTocUrl` 翻页（0/1/>1 三条分支）
- [ ] 正文抓取：`ruleContent.content`（多值以 `\n` 拼）+ `nextContentUrl` 分页循环 + 遇「下一章链接」break
- [ ] 逐字段容错：单字段失败只记日志不中断；`name` 为空即丢弃整条（legado 语义，**必须保留**）
- [ ] 净化：`HtmlFormatter` 等价实现（`&nbsp;`→空格、块级标签→换行、去注释/标签、`\s*\n+\s*`→`\n　　`）
      + `replaceRegex`（正则带 3s 超时、**超时禁用规则**）+ 段落缩进
- [ ] **防死循环加固（比 legado 强）**：分页/翻页循环 = 已访问 URL 集合 **且** 最大 50 页

**测试**
- [ ] `fixtures/sources/*.json` + `fixtures/pages/*.html`（离线快照）
- [ ] **手写一个最小文本书源**作为主 fixture（`bookSources.json` 里那个是音频压力源，见 §8 附录）
- [ ] 必须覆盖的易错点：① `{{}}` 在 URL vs 规则里的不同语义；② `[attr="a&&b"]` 不能被切碎；
      ③ `@js:` 吃满剩余字符串；④ 子规则「对象或字符串」双形态

- **验收**：`cargo test -p novel-core` 全绿；`novel-cli fetch <source.json> <bookUrl> -c 3` 能打印正文。

> 已确认无需自研：`updateTime` 不必解析（legado 也只当字符串）；`isVip` 就是 `is_true()`。
> 我们**额外**做一件事：把 `updateTime` 解析成时间戳另存字段，用于「更新 N 章」提示。

### P2 — 阅读器成型
- [ ] `render.rs`：章节 → TS 文本（模板可配置）
- [ ] 章节目录（用 `documentSymbol` 或 codeLens 列表）、跳转
- [ ] 阅读进度持久化（`store`：当前书 + 章节 index），重开恢复
- [ ] 快捷键：Zed 侧用 codeLens 点击即可，不依赖自定义快捷键

### P3 — 多书源搜索 + 换源 + 缓存

**已确认的 legado 策略**（`model/webBook/SearchModel.kt`）：

| 项 | legado 做法 | 我们的做法 |
|---|---|---|
| 并发原语 | `flatMapMerge(32)` + `newFixedThreadPool(min(32, MAX_THREAD=9))` | `Semaphore(N)` + `JoinSet` |
| 并发度 | 在途请求 32；9 个线程只限 CPU 侧解析 | `--concurrency`，默认 8（桌面端更保守） |
| 单源超时 | 硬编码 **30s** | 可配，默认 30s |
| 失败处理 | `catch(_: Throwable){ ensureActive() }` **全吞**，该源静默 0 条 | 同样吞，但**上报每源事件** |
| 源级限速 | `concurrentRate` = `"2/1000"` 或单数字，超限 `delay` 自旋 | 每源一把 `Mutex` + `last_request_at`，等价语义 |
| 全局信号量 | **无** | 有（我们要限总带宽） |
| 源排序 | `customOrder asc`，**不用 weight**，不按 type 过滤 | 同 |
| 进度回调 | 每完成一源回调**整份合并列表**（非 diff），无「第 n 源」事件 | 传 diff + 每源事件（`n/N 书源完成`） |
| 取消 | 新关键字 → `cancelSearch()` 取消 job + 关线程池 | 关键字变更即 `CancellationToken::cancel()` |

- [ ] `search.rs`：`Semaphore` + `JoinSet` + `select!{ cancel, timeout(t), search_one }`，`mpsc` 增量上报
- [ ] 源级限速器：解析 `concurrentRate`，`"n/ms"` 与单数字两种格式
- [ ] 每字段容错：单字段解析失败只记日志不中断（legado 语义，必须保留）；`name` 为空即丢弃整条
- [ ] 单源内去重：`LinkedHashSet` by `bookUrl`
- [ ] **跨源合并** `merge_into`：key = `(name, author)`，命中则 `origins.insert(sourceUrl)`；排序 = 4 桶优先级（name==key / author==key / kind 含 key / 其他）+ 桶内 `origins.len()` 降序
- [ ] 换源：按 `name` + `author` 匹配（**不是** `bookUrl`），join 书源表按 `customOrder` 排序
- [ ] 搜索 UI：把结果渲染成另一个 TS buffer（每条 = 一个可点击条目），或复用阅读 tab 的目录区
- [ ] SQLite：书源表、书架表、章节元数据表（见 P4）

### P4 — 离线下载与缓存

**已确认的 legado 模型**：

- **正文存文件，不存 DB**：`book_cache/<书名前9字>_<md5_16(bookUrl)>/<index5位>-<titleMD5>.nb`，key 实质 = `(bookUrl, index)`（`help/book/BookHelp.kt:176-195`、`BookChapter.getFileName():186-190`）。
- **元数据在 Room `chapters` 表**：主键 `(url, bookUrl)`，索引 `(bookUrl, index)` 唯一（`data/entities/BookChapter.kt:31-53`）。
- **`Cache.kt` 与正文无关**：是通用 KV（`key/value/deadline`），给书源变量/JS 用。别混淆。
- **任务粒度**：一个书一个 model（`cacheBookMap: bookUrl→model`），任务是**章节 index 区间**。
- **并发**：多书并行（`onEachParallel(threadCount)`），**单书串行**（每轮取 `waitDownloadSet.first()`）。
- **失败重试**：计数 `< 3` 重新入队，失败后 `delay(1000)`，**无指数退避**。
- **暂停/断点**：状态全在内存，暂停即清空集合；断点靠「文件已存在即跳过」。**重启不续传队列**。
- **没有章节正文缓存失效机制**：无 TTL、无规则版本号，`hasContent` 只看文件是否存在。

我们的实现：
- [ ] 存储：`novel_cache/<book_id>/<index>.txt` + SQLite 单表 `chapters(book_id, idx, title, url, is_vip, tag, word_count, downloaded, fail_count)`
- [ ] 队列：全局 `Semaphore(4~8)` + `HashMap<book_id, VecDeque<i32>>`（单书串行）
- [ ] `fail_count` **落库**，重启可续传（**比 legado 好**：它队列在内存，重启丢失）
- [ ] 重试：3 次 + 1s 延迟（够用）
- [ ] **缓存加规则集版本号**进路径（legado 没有，规则改了会读到脏缓存 —— 这是它的坑）
- [ ] 进度上报（Zed 侧走 `window/showMessage` / `workDoneProgress`）
- [ ] 已下载书籍离线阅读（跳过网络）
- [ ] 砍掉：阅读时预下载（`durChapterIndex ± 2`）、图片章节缓存、`CacheManager` 三级 LRU

### P5 — 可选增强
- [ ] `@xpath`、`@js`（嵌入 JS 引擎）、登录 cookie、书源订阅更新、替换净化规则集

## 7. 风险与待验证

| # | 风险 | 状态 | 应对 |
|---|---|---|---|
| R1 | Zed 扩展无法开 buffer / 写文件 | ✅ **已确认无法** | 走 LSP 反控（§2.2）；扩展只负责装二进制 |
| R2 | `applyEdit` 不支持无磁盘虚拟 buffer | ✅ **已确认不支持** | 固定路径落盘 `.novel/reader.novel`；不能用 `untitled:` 虚拟 buffer |
| R3 | LSP 主动请求（server→client）支持不全 | ⚠ 已验证支持，但**依赖版本** | P0.0 先查 Zed 版本 ≥ #63607；三层降级见 §6 P0 |
| R4 | 规则引擎工作量被低估（legado 支持 JS/XPath） | ⚠ 已知 | MVP 砍到 `@css`+`@json`+`@@`；无 JS 的源**显式报不支持**，绝不静默返回空 |
| R5 | 反爬（UA/cookie/编码/JS 加密） | ⚠ 已知 | header 透传 + `encoding_rs`/`chardetng` + cookie store；JS 加密源直接放弃 |
| R6 | 版权/合规 | ⚠ 已知 | 只做工具，不内置书源；书源由用户自行导入 |
| **R7** | **`.ts` 后缀会拉起真正的 `typescript-language-server`**，对伪 TS 报诊断 + 白耗资源 | 🆕 新增 | 用自定义 language `novel` + `.novel` 扩展名，但引用 TypeScript 的 tree-sitter grammar（高亮仍是 TS） |
| **R8** | **章节文件在 worktree 外 → 可能开新窗口**（`WorkspaceMatching::MatchExact`） | 🆕 新增 | 写进 `<worktree>/.novel/`，**绝不**用系统临时目录 |
| **R9** | **LSP 启动时机**：不打开匹配语言的文件时根本不启动 | 🆕 新增，**待实测** | 这就是为什么触发靠 `zed -a .novel/reader.novel`；P0.2 专门验证 |
| **R10** | `download_file` 需要用户授权 capability；平台判断只有 `(os, arch)`，**无 libc 变体**，且 32 位 x86 支持已移除 | 🆕 新增 | 只发布 `linux/macos/windows × x86_64/aarch64` 四个 target |
| **R11** | `process::run_command` 是阻塞的，**扩展无法 spawn 守护进程** | 🆕 新增 | 长驻进程只能靠 LSP 启动 —— 与 §2.5 一致 |

## 8. 调研回收情况

- [x] **多书源搜索并发/超时/去重/进度策略** → §6 P3（`SearchModel.kt` 全套已确认）
- [x] **正文分页与净化顺序、下载队列模型** → §4.4/§4.5 与 §6 P4
- [x] **书源字段/规则语法精确清单** → §4 全节 + 附录 A/B/C
- [x] **Zed 扩展 API 能力与 LSP 反向控制证据** → §2 全节（基于 zed@main 源码 + PR/issue）

### 已交付的调研报告（作为附录保留）

| 代理 | 内容 | 回填位置 |
|---|---|---|
| `legado-search` | 多书源搜索链路 | §6 P3 |
| `legado-content` | 章节列表 → 正文 → 净化 → 缓存/下载 | §4.4/§4.5、§6 P4 |
| `legado-source` | 书源 schema + 规则引擎 | §4、附录 A/B/C |
| `zed-capability` | Zed 扩展能力边界 | §2、§6 P0、§7 |

### 跳过未调研的部分

Android UI 层、Rhino JS 引擎细节、音频/订阅/漫画/视频分支、发现页（`exploreUrl`）、
段评（`ruleReview`）、`SourceVerificationHelp` 人机验证、TTS/朗读。

---

## 9. 三条明确要做得比 legado 好的地方（不要丢）

1. **分页/翻页循环加上限**：legado 只靠已访问 URL 集合防死循环（`BookChapterList.kt:72`、
   `BookContent.kt:73-102`），一旦站点返回递增 URL 就会无限抓。我们加 `max_pages = 50`。
2. **下载队列落库**：legado 队列在内存（`waitDownloadSet`），进程死了就丢；我们把
   `fail_count` / 待下载区间写 SQLite，重启续传。
3. **缓存带规则版本号**：legado 正文缓存 key = `(bookUrl, index)`，与书源规则无关；
   用户改了 `contentRule` 后旧缓存仍命中，正文是脏的。我们在缓存路径里带 `source_rule_hash`。

### 三个必须写单测的易错点

1. `{{ }}` 在 `AnalyzeUrl`（一律当 JS）与 `AnalyzeRule`（内容像规则就当规则）里**语义不同**。
2. `[attr="a&&b"]` 不能被 `&&` 切分器切碎（平衡组切分）。
3. `@js:` **吃满到字符串末尾**，之后的规则会被误当规则解析。

## 附录 A：仓库内置的唯一书源（压力型 fixture）

`app/src/main/assets/defaultData/bookSources.json`，单元素数组，5.2 KB。这是个**音频源**，
非常适合做「复杂特性冒烟测试」，但**不适合做最小 fixture**。它同时用到：
`bookSourceType:1`、POST + JSON body、`@js:` 长链、`java.put/get` 跨规则传参、
URL 内 `{{…}}` JS、`{{searchPage}}` 旧写法、`loginUrl`/`loginCheckJs`/`loginUi`、`payAction`。

```json
[{
 "bookSourceName": "消消乐听书",
 "bookSourceType": 1,
 "bookSourceUrl": "https://www.kaixin7days.com",
 "searchUrl": "https://www.kaixin7days.com/book-service/bookMgt/findBookName,{\"method\":\"POST\",\"body\":{\"title\": \"searchKey\",\"pageNum\": {{searchPage}},\"pageSize\": 100}}",
 "ruleSearch": {
  "bookList": "$.content.content",
  "name": "$.title",
  "author": "$.author",
  "intro": "$.desc",
  "lastChapter": "$.newestChapter",
  "bookUrl": "$.id@js:java.put('bookId', result);'https://...getAllChapterByBookId,{ \"method\": \"POST\",\"body\": {\"bookId\": \"'+result+'\",\"pageNum\": \"1\",\"pageSize\": \"10000\"} }'",
  "coverUrl": "$.cover@js:var cover = JSON.parse(result);'https://...getPicture?filePath='+cover.storeFilePath"
 },
 "ruleToc": {
  "chapterList": "$.content.content",
  "chapterName": "$.chapterTitle",
  "chapterUrl": "$.id@js:java.put('chapterId', result);'...'"
 },
 "ruleContent": { "content": "" }
}]
```

> 全文（含完整 JS）在 `reference/legado-E-main/app/src/main/assets/defaultData/bookSources.json`，
> 需要时直接拷进 `fixtures/`。

**建议的最小文本书源 fixture 形状**（自己填真实站点后再冻结为快照）：

```json
{
  "bookSourceUrl": "https://example.com",
  "bookSourceName": "示例源",
  "searchUrl": "/search?q={{key}}&page={{page}}",
  "ruleSearch": {
    "bookList": ".result li",
    "name": ".title@text",
    "author": ".author@text",
    "bookUrl": ".title@href"
  },
  "ruleToc": { "chapterList": "#list dd", "chapterName": "a@text", "chapterUrl": "a@href" },
  "ruleContent": { "content": "#content@html" }
}
```

## 附录 B：明确移植、但推迟的 legado 特性

| 特性 | 为何推迟 |
|---|---|
| `@js:` / `<js>` / `jsLib` / `{{js}}` | 需嵌 JS 引擎（`boa_engine` / `rquickjs`）。先支持 `java.put/get` 极小子集，其余源**显式标记不支持并跳过**，绝不静默返回空（否则误判「书源失效」） |
| `@xpath:` | 文本源里占比低，引入 `libxml`/`sxd-xpath` 成本高 |
| JsonPath 完整语法 | 先手写实现 `$.a.b[0]` 子集 |
| `%%`、`@webjs`、WebView、`:` 正则模式 | 罕见 |
| `ruleReview`、`font`/`imageDecode`/`payAction`/`callBackJs`、`loginUi` | 与阅读器定位无关 |
| `SourceVerificationHelp` 人机验证 | 需 WebView 交互，桌面端无此能力 |

## 附录 C：书源生命周期（`help/source/SourceHelp.kt`）

- 去重键 = `bookSourceUrl`（`BookSource.kt:83-91`）；DAO `@Insert(onConflict = REPLACE)`（`BookSourceDao.kt:258-259`）
- 导入前用 `assets/18PlusList.txt` 的 base64 域名黑名单过滤（`SourceHelp.kt:141-169`）—— 我们可不做
- 删除时级联清理 `cacheDao.deleteSourceVariables` 与 `SourceConfig`（`:91-95`）
- 插入后 `adjustSortNumber()` 在 `customOrder` 越界/重复时重排（`:174-186`）
- `getSource(key)`：先取内存中的当前阅读源，再查 DB（`:42-55`）
- `exploreKinds()` 按 `md5(bookSourceUrl+exploreUrl)` 缓存，`Mutex` 防并发重算（`BookSourceExtensions.kt:31-105`）—— 发现页不做

---

## 10. P0 实施记录（已完成）

### 落地结构

```
crates/novel-core/src/render.rs    章节 → TS 文本 + codeLens 锚点（同一个函数产出，不会脱节）
crates/novel-core/src/sample.rs    P0 硬编码 3 章假书
crates/novel-lsp/src/lsp.rs        LSP 会话（手写 JSON，不依赖 lsp-types）
crates/novel-lsp/tests/lsp_session.rs  端到端跑真实子进程的 LSP 会话
zed-extension/                     独立 workspace（wasm32-wasip2），已能编译
.zed/settings.json                 临时劫持 TypeScript（见下）
```

### 三个偏离原计划的决定

1. **P0 不装扩展，改为 `.zed/settings.json` 劫持 TypeScript。**
   `"file_types": { "TypeScript": ["novel"] }` + 把 `typescript-language-server` 的
   binary 换成 `novel-lsp`（并屏蔽 Zed 默认的 `vtsls`）。
   白拿 TS tree-sitter 高亮，零扩展安装步骤 —— 而扩展安装**必须点 UI**，无法脚本化。
   扩展文件已写好并能编译，等 P0.5 换成它。
2. **不需要 `novel-cli` crate。** 一个二进制，默认身份是 LSP。
   等 P1 真的需要无 Zed 调试时再加。
3. **章号从正文里解析反推**（`export const chapter: ChapterRef = { index: N`），
   这样 LSP 重启后能接着上次读，不用先上 SQLite。

### 实测发现

- **`code_lens` 在 Zed 里默认是 `off`**（`assets/settings/default.json:444`），
  必须显式 `"on"` 才看得到按钮。`"menu"` 是第三种模式（收进 code action 菜单）。
- TypeScript 在 Zed 默认是 `["!typescript-language-server", "vtsls", "..."]` ——
  `typescript-language-server` 是被**显式屏蔽**的，所以劫持要同时改 `languages` 段。
- wasm 里 `std::fs` 看不到宿主文件系统，扩展没法 `exists()` 探测二进制路径，只能直接给出路径。

### 测试

`cargo test` → 8 passed。其中 `tests/lsp_session.rs` 起真实子进程跑完整协议往返，断言
`didOpen → applyEdit+showDocument`、`executeCommand → 换章`、替换范围覆盖整篇旧内容、
codeLens 锚点落在 `export function` 行上、磁盘内容与 `applyEdit` 一致。

### P0 尚未验证（需要人在 Zed 里点一次）

- `window/showDocument` 是否真的聚焦同一个 tab（本机 Zed 1.20.2 应该可以）
- codeLens 在 Zed 里的可见性与点击手感
- `.novel/` 路径在当前 worktree 内 → 不会开新窗口

---

## 11. P0 修正：去掉对 Zed 的依赖（重要）

### 起因

首次真实联调（Zed 1.20.2）暴露两件事：

1. **`window/showDocument` 不被支持** —— Zed 记 `sent unhandled notification`。
2. **`zed -a <文件>` 不可靠** —— 用户按 F4 跑任务后无反应。手动跑同一条命令却能生效，
   说明 `zed -a` 依赖「当前哪个窗口是活跃的」，在多窗口 + 焦点不稳时目标不确定。

### 结论：翻章不要请求 Zed 干任何事

原来设计是 `applyEdit`（改 buffer）+ `showDocument`（聚焦）。改成：

> **只写磁盘上的文件。永不发 `workspace/applyEdit`。**

因为 **buffer 保持「干净」→ 磁盘一变 Zed 自己重载**。这一个决定同时消掉四个风险：

- 不需要任何 Zed 未实现的能力
- 不会留未保存修改 / 「文件已在磁盘上改变」冲突弹窗
- 外部写入方（命令行、别的编辑器）能和 codeLens 并存，不打架
- 多窗口也不影响：文件改动会反映到所有打开它的窗口

唯一还用的 server→client 请求是 `window/showDocument`（把光标拉回章首），
而且**只在客户端声明了 `window.showDocument.support` 时才发** ——
Zed 1.20.2 没声明，所以日志里不会再出现 unhandled 噪音。

### 于是有两条完全独立的翻页通路

| 通路 | 触发 | 链路 |
|---|---|---|
| ① codeLens | 点文件里的 `▶ 下一章` | `executeCommand` → 写文件 → `codeLens/refresh` |
| ② 命令行 | 任务 `novel: 下一章` | `novel-lsp next` → 写文件 |

② 完全不经过 LSP，所以就算 ① 因为 Zed 版本/配置挂掉，阅读器照样能用。

### 其他改动

- **导航块顶部 + 底部各一组**（6 条 codeLens）。因为没法把光标拉回章首，
  「读完这一章手边就有下一章」的按钮比顶部的更有用。
- **新增 `novel-core::pager`**：`.novel/reader.novel` 这个文件本身就是真相。
  章号存在 `export const chapter` 那一行，LSP 重启和命令行新进程都从文件反推，
  不需要额外持久化。
- **一个二进制两个身份**：无参数 = 语言服务器；`next`/`prev`/`goto`/`show` = 命令行翻页器。
  （仍然不需要单独的 `novel-cli` crate。）
- **Zed 任务不转义引号** → `command` 必须无空格。用
  `C:\Users\Administrator\bin\{zed-novel-open,novel}.cmd` 两个转发脚本。

### 测试（13 passed）

`tests/lsp_session.rs` 覆盖：只写盘不发 applyEdit、点 next 换章且旧正文清干净、
点 prev 逐字节还原、6 条 codeLens 锚点正确且底部那组在后 1/3、
showDocument 按客户端能力开关、命令行 next/prev/goto/越界。

### 剩下的唯一未知

**Zed 是否真的会在磁盘文件被外部修改时重载「干净」的 buffer。**
这是新架构的地基，只能人工验证：点一次 `▶ 下一章`，看正文有没有换。
若日志里出现 `didChange`，就说明重载发生了（我们把 didChange 当探针在记日志）。

---

## 12. P0.2：控制面板 = 同一个 tab 的多个页面

### 需求

> 「一个独立的小窗口完成配置、搜索、选择、章节选择；阅读器部分只负责章节切换；
> 不通过 run 面板启动；挂在 Zed 左侧一个小入口。**以 Zed 扩展的形式加入**。」

### 硬约束（已核实，见 §2）

Zed 扩展是 `wasm32-wasip2` 沙箱，**没有任何 UI API**：
- ❌ 不能贡献 dock 面板（`ExtensionManifest` 里没有 panel 字段，WIT 里没有 UI 接口）
- ❌ 不能开窗口、不能开 buffer
- ❌ 不能写 worktree 文件（只能写自己 workdir）
- ❌ `process::run_command` 是阻塞的，不能 spawn 守护进程

扩展能碰的只有：**自定义语言 + 语言服务器**（+ slash command / MCP / 主题 / DAP）。

### 于是「控制面板」只能是 LSP 能表达的形状

| 想要的 | 在 Zed 里能用的形状 |
|---|---|
| 一个可读列表 | 文件里的一段 `//` 文本 |
| 列表里点一项 | 那一行上面的 codeLens |
| 点一下执行动作 | codeLens → `workspace/executeCommand` |
| 刷新界面 | 重写文件 + `workspace/codeLens/refresh` |
| 文字输入（搜索词） | 任务/终端参数，或选中文字后跑任务 |

**结论：五个页面复用同一个 tab** —— 书架 / 目录 / 正文 / 搜索 / 配置。
零额外窗口，纯键盘可用，而且界面就是文件本身（LSP 没跑时也能当清单读）。

### 交互锚点规则（这是本节的实质）

| 页面 | codeLens 锚在哪 | 为什么 |
|---|---|---|
| 正文页 | `export function nextChapter(): Command {` | 按钮和它操作的函数是同一处，不可能错位 |
| 目录/书架/搜索/配置 | 那一行 `//` 清单文字 | 文件本身可读；按钮在它所描述的那一行旁边 |

底部导航用**清单行**而不是函数桩 —— 同一页不能出现重名函数（生成的 TS 会挂），
而且「下一章是哪章」应该一眼看得懂。

### 状态存在文件里

```ts
export const state: UiState = {"kind":"reader","book":0,"chapter":2,"tocOffset":0,"query":"","enabled":[true,true,true]};
```

页面/书号/章号/目录滚动位置/搜索词/书源开关全在里面。LSP 重启、命令行改文件、
编辑器重载，三边看到的永远是同一份真相，**不需要额外持久化**（P2 有需要再上 SQLite）。

### 命令表（12 个，固定）

`novel.shelf` `novel.toc` `novel.reader` `novel.search` `novel.config`
`novel.goto` `novel.open` `novel.source.toggle`
`novel.prev` `novel.next` `novel.toc.prev` `novel.toc.next`

带参数的命令（`goto`/`open`/`toggle`）把参数放在 codeLens 的 `command.arguments` 里 ——
服务端不用猜是第几章，也不用为每个章节注册一个命令。

### 目录分屏

`TOC_PAGE_SIZE = 200`。一本三五千章的书全铺出来就是三五千条 codeLens，Zed 会卡死。
打开目录时会自动滚到当前章所在那一屏（`tocOffset = (chapter-1) / 200 * 200`）。

### 三级降级

| 通路 | 触发 | 依赖 |
|---|---|---|
| ① 点按钮 | codeLens | LSP + `code_lens: "on"` |
| ② 任务 | `novel: 书架/目录/搜索选中内容/下一章` | 只要 PATH 上有 `novel.cmd` |
| ③ 终端 | `novel-lsp <cmd>` | 什么都没依赖 |

②③ 完全不经过 LSP。①因为 Zed 版本/配置挂掉时，阅读器照样能用。

### 测试（20 passed）

集成测试逐个点按钮，每次断言**文件里的状态**变了（不是只看返回值）。
新增两条单元不变量：

1. **同一页里 `export function` 名字不能重复** —— 重复了生成的 TS 直接编译不过。
   （这条当场抓出了书架页两个 `openSearch`、正文页上下两个 `nextChapter`。）
2. **每页都能在 LSP 缺席时当纯文本读** —— 书架能看到书名作者、目录能看到章标题。

### 仍待确认

- 「挂在 Zed 左侧的小入口」**在扩展里做不到**（无 panel API）。
  目前最接近的替代：项目面板里的 `.novel/reader.novel` 文件当入口 + 任务快捷键。
  正在等一轮调研确认 Outline Panel（左 dock）能否当目录导航。
- Zed 是否真的会在磁盘文件被外部修改时重载「干净」的 buffer（`didChange` 探针）。

---

## 13. 「挂在 Zed 左侧」的调研结论（已核实）

### 结论：**扩展无法往左侧 dock 加面板。明确不可行。**

三条硬证据（Zed 源码 1.22 快照 + 1.20.2 本机验证）：

1. **面板集合是硬编码的**：`crates/zed/src/zed.rs:776-812` 的 `initialize_panels()` 里
   只有 7 个 —— `ProjectPanel` / `OutlinePanel` / `TerminalPanel` / `GitPanel` /
   `CollabPanel` / `DebugPanel` / `AgentPanel`。没有第 8 个可注册位。
2. **`ExtensionManifest` 零 UI 字段**（`crates/extension/src/extension_manifest.rs:84-115`）：
   只有 id/name/version/description/repository/authors + `lib`/`themes`/`icon_themes`/
   `languages`/`grammars`/`language_servers`/`context_servers`/`slash_commands`/`snippets`/
   `capabilities`/`debug_adapters`/`debug_locators`/`language_model_providers`。
   WIT world（含最新的 `since_v0.8.0`）同样零 UI 接口。
3. **settings 里没有「自定义面板/按钮」配置项**；上游 **#21208「Webview via Extensions」仍 open**
   （456 reactions），#18043（buffer 控制 API）也 open。

**顺带更正两处我之前的说法：**

- `process:exec` 能力 + `process.run-command` **可以** spawn 进程（所以扩展不是「完全不能执行进程」），
  但它是**阻塞式**、等进程退出才返回 stdout/stderr，所以无法保活守护进程。
  （想让子进程脱离，只能用 `cmd /c start` 这类启动器把它甩出去。）
- `applyEdit` 我原本以为靠 PR #63607 —— 该 PR 2026-09-09 合并，**1.22 才有**，
  `window/showDocument` 也是那时补的。本机 1.20.2 确实没有，与我们实测一致。

### 默认 dock 位置（`assets/settings/default.json`）

| 面板 | 设置键 | 默认 |
|---|---|---|
| 项目面板 | `project_panel.dock` | `right` |
| Outline | `outline_panel.dock` | `right` |
| 协作 / Git | `collaboration_panel.dock` / `git_panel.dock` | `right` |
| **Agent** | `agent.dock` | **`left`** |
| 终端 | `terminal.dock` | `bottom` |
| 调试 | `debugger.dock` | `bottom` |

→ **唯一默认就在左侧的是 Agent 面板**，而扩展只能通过 slash command 往里插文本，与阅读器无关。

### 四条替代路径（按推荐度）

| 方案 | 常驻左侧 | 代价 |
|---|---|---|
| ① 项目面板停左 + `.novel/reader.novel` 当入口（**现状**） | ✅ | 语义像文件不像按钮；项目面板条目只能来自真实文件系统（`project_panel.rs:95-99`），做不了「虚拟条目」 |
| ② **终端面板停左 + 常驻面板**：`terminal.dock:"left"` + 任务的 `label`/`reveal:"always"`/`hide:"never"`/`reveal_target:"dock"` + 键位绑 `["task::Spawn",{"task_name":…}]` | ✅ | 需要一次触发启动；终端不是原生 UI |
| ③ Outline 面板停左当导航（**已实现**） | ✅ | 点击**只能把光标跳过去，不能执行命令**（`outline_panel.rs:2897-2914`）；且只能显示当前 buffer 的符号 |
| ④ 快捷键 + codeLens（**已实现**） | ❌ | 不在左侧 |

### 本轮实际落地的三件事

**1. `documentSymbol` + Outline 停左（方案③）**

`novel-core::ui::Rendered` 现在同时产出 `lenses` 和 `symbols` —— 每个 codeLens 锚点
也是一个文档符号。于是：

- 正文页的 Outline 列出 `prevChapter` / `nextChapter` / `openToc` / `openShelf` / `openConfig`
- **目录页的 Outline 列出全部章节标题** ← 左侧变成章节导航器

`.zed/settings.json` 里开了 `outline_panel.dock: "left"` 和
`languages.TypeScript.document_symbols: "on"`。

⚠ `document_symbols` 是**语言级**设置，所以显式限定在 `TypeScript` 下 ——
设成顶层会把所有语言的 outline 都改成走 LSP，那是个不该有的副作用。

**2. 快捷键（方案④，同时解决「不通过 run 面板启动」）**

`task::Spawn` 带 `task_name` 时，只要 label 精确命中就直接执行；
**只有一条都没匹配上才退回任务选择弹窗**（`tasks_ui.rs:185-215`）。
所以把 label 写成唯一且精确，就真的不会弹面板。

新增 `.zed/keymap.json`，前缀 `ctrl-alt-n`（已核对 Zed 默认键位里是空的）：

| 键 | 动作 |
|---|---|
| `ctrl-alt-n` | 打开阅读器 |
| `ctrl-alt-n n` / `p` | 下一章 / 上一章 |
| `ctrl-alt-n t` / `b` / `c` | 目录 / 书架 / 配置 |
| `ctrl-alt-n s` | 搜索选中内容（绑在 Editor 上下文，因为要读选区） |
| `ctrl-alt-n d` | 当前页 |

**3. 配置一致性测试（`crates/novel-lsp/tests/zed_config.rs`）**

「keymap 的 `task_name` 必须精确等于 tasks 的 `label`」这种约束最容易在改名时
悄悄坏掉，坏掉的表现又恰好是「弹出了用户不想要的 run 面板」。所以上了三条测试：

- 每个 `task::Spawn {task_name}` 都命中真实 label；label 不重复；没有重复键位
- 所有任务都是 `reveal: "never"` + `use_new_terminal: false`（否则每次翻页都跳终端）
- 所有 `command` 都不含空格和路径分隔符（Zed 拼 shell 命令不转义引号）

### 还缺的输入

`reveal` 的合法值是 `always`(默认)/`no_focus`/`never`，**没有 `on_failure`**；
另有 `hide`(`never` 默认/`always`/`on_success`) 和 `reveal_target`(`dock` 默认/`center`)。
方案②（左侧常驻面板）要用 `reveal:"always"` + `hide:"never"` + `reveal_target:"dock"`
—— 正在等一轮「终端里跑 TUI 在 Zed/Windows 上是否可靠」的调研，再决定是否引入
ratatui/crossterm 依赖。

---

## 14. 左侧面板的三条实现路线（数据 + 决策）

### 路线 A：Zed 终端里跑全屏 TUI（**唯一符合「扩展形态」的方案**）

Zed 终端 Windows 后端 = 自 fork 的 `alacritty_terminal` + `conpty`
（`alacritty_terminal/src/tty/windows/conpty.rs` 的 `CreatePseudoConsole`/`ResizePseudoConsole`）。
能力面（`crates/terminal/src/terminal.rs`）有 `Modes::ALT_SCREEN`、`MOUSE_MODE`、
`SGR_MOUSE`(1006)、鼠标转发（Shift 绕过应用捕获）。官方文档也承认 TUI 是预期用途
（`line_height: "standard"` 注释写着「better for TUI applications with box-drawing characters」）。

**但有三个已知硬伤：**

| 问题 | 证据 |
|---|---|
| **Windows 上全屏重绘型 TUI 有重复帧/残影，且 resize 会加剧** | [#52184](https://github.com/zed-industries/zed/issues/52184) **open**，同一会话在 Windows Terminal 不复现 |
| **Zed 截走终端里的 `Ctrl+Shift+C/V/F/L`** | 官方文档 `zed.dev/docs/terminal` —— TUI 收不到这些键 |
| crossterm 在 Win11 上鼠标事件丢 Shift 修饰符（#986）、不支持横向滚动（#466）、不做 ANSI/VT 输入解析（#1030 open） | crossterm issue 列表 |
| （附）CJK 单元格算术是对的，但字形图集双线性模糊 | [#57777](https://github.com/zed-industries/zed/issues/57777) open |

同步输出不是问题：fork 的 `term/mod.rs` 里 `NamedPrivateMode::SyncUpdate => ()` 是 no-op，
但 pin 的 `vte 0.15` 的 `Processor` 已做字节级缓冲 → 残影更可能来自 Zed 自身渲染/conpty。

**依赖成本**：`ratatui 0.30.2` + `crossterm 0.29.0`；`tuirealm` 是 ratatui 之上再包一层（更重）；
**只引 crossterm 手写是最省的**。

### 路线 B：独立原生窗口（eframe/egui）—— **不是扩展形态**

本机实测：

| 后端 | 唯一依赖 | 编译 crate | 冷编译 | release 二进制 |
|---|---|---|---|---|
| `eframe 0.32` + **glow（默认）** | **116** | 129 | **1m53s** | **3.49 MB**（opt-level=z+lto+strip） |
| `eframe 0.32` + wgpu | 140 | 155 | 2m20s | — |

- **中文要自己管**：egui 自带字体只有 Hack / Ubuntu-Light / NotoEmoji，**没有 CJK**，
  必须 `include_bytes!` 或读 `C:\Windows\Fonts\msyh.ttc` 注入。
- **无边框小面板可行**：`ViewportBuilder` 支持 `with_decorations(false)` /
  `with_always_on_top()` / `with_min_inner_size` / `with_resizable` 等。
- **单实例要自己做**：eframe 无内置；需 `single-instance`（命名 mutex + IPC）。
- **聚焦不可靠**：`ViewportCommand::Focus` → winit `focus_window()`，Windows 上受
  `SetForegroundWindow` 前台锁定限制（[MS 文档](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-setforegroundwindow)：
  「An application cannot force a window to the foreground while the user is working with
  another window. Instead, Windows flashes the taskbar button」）。
  `AllowSetForegroundWindow` 只对**新**进程有效，对已存在进程无效 → 没有 100% 可靠方案。

**依赖对比（实测 `cargo tree` 唯一依赖数）**：
`fltk(fltk-bundled)` **12** < `egui/eframe` 116 < `iced` 143 < `slint` 241。

### 路线 C：复用已有环境

- **浏览器当 UI**（Rust 起 `TcpListener` + 打开默认浏览器）：零 GUI 依赖，最快。
  缺点：窗口不可控（新标签、无法置顶/无边框/定位）、焦点不在 Zed、无键盘集成。
- **本机 Qt 5.15/6.10**（`D:\devtools\Qt`）：中文与 Windows 稳定性都无风险，
  代价是 CMake + windeployqt 另一条工具链。
- **让 Zed 自己显示**：❌ 已确认不行（§13）。

### 决策

用户要求「以 Zed 扩展的形式加入」→ **只有路线 A 符合**（伴生二进制跑在 Zed 终端面板里，
面板停左 = 左侧常驻入口）。路线 B/C 都是独立程序，不是扩展。

但 A 有 #52184 这个未修复的渲染风险，而它恰好是「全屏重绘型 TUI」最怕的。
**所以在引 ratatui/crossterm（116 个依赖的一条路）之前，先用零依赖探一次。**

### 已落地：零依赖探测程序（`novel-lsp probe`）

`crates/novel-lsp/src/probe.rs`，纯 ANSI 序列，**没有任何新依赖**：

- 进备选屏、隐藏光标、画一个含中文 + 框线 + 颜色 + 反显的框
- 轮流跑三种重绘模式（各 8 秒），因为它们触发的渲染路径不同：
  `增量`（只定位 + `ESC[K` 清行尾，**ratatui 的 diff 渲染就是这样**）、
  `全清`（每帧 `ESC[2J`）、`高刷`（30fps 增量）
- 每帧重新读 `COLUMNS`/`LINES`，所以**拖拽面板改变大小时会立刻反映**
- 无论正常结束还是出错都恢复终端

配套任务 `novel: 终端渲染探测`（`reveal:"always"` + `hide:"never"` +
`reveal_target:"dock"` + `use_new_terminal:true`）**同时验证两件事**：
① TUI 渲染有没有残影 ② 「终端面板停左 + 常驻」这条配方好不好用。

`.zed/settings.json` 里已加 `"terminal": { "dock": "left" }` 配合这个实验。

### 顺带修掉的一个测试 bug

`zed_config.rs` 里的 jsonc 注释剥离器**按 byte 迭代再 `as char`**，
把中文 UTF-8 编成了 Latin-1 乱码。它坏得「两边一样」，所以
`task_name == label` 那几条测试照样通过 —— 差点蒙过去。
改成按 `char` 迭代，并加了一条专门抓这类损坏的测试
（`comment_stripping_preserves_non_ascii_text`）。

---

## 15. 原生按钮：扩展能拿到的全部 UI 面

用户要求「融合进去，插件本身应该有一些按钮」。查证后的结论是：
**扩展没有 UI API，但语言服务器能让 Zed 原生渲染出一整套可点控件。**
这些不是自绘的假按钮，是 Zed 自己的控件：

| 控件 | LSP 请求 | Zed 侧实现 | 交互 |
|---|---|---|---|
| 行上方的内联按钮 | `textDocument/codeLens` | `editor/code_lens.rs` | 点击 → `workspace/executeCommand` |
| 行内的可点文本 | `textDocument/inlayHint`（label part 带 `command`） | `element/mouse.rs:967` → `activate_hovered_inlay_hint_command` | **普通左键**（无修饰键），鼠标变手指 |
| 原生菜单 | `textDocument/codeAction` | `editor::ToggleCodeActions`（`ctrl-.`） | 选中 → 派发命令 |
| 带按钮的通知 | `window/showMessageRequest` | toast，按钮是 `actions[].title` | 点按钮回 `MessageActionItem` |
| 左侧列表 | `textDocument/documentSymbol` | Outline 面板 | **只能跳光标**，不能执行命令 |
| 可点文本链接 | `textDocument/documentLink` | `hover_links` | 打开 `target` URL（`lsp_document_links` 默认 true） |
| 补全菜单 | `textDocument/completion` | — | 插入文本 |
| 状态栏进度 | `window/workDoneProgress/create` | — | 非交互 |

**关键发现（本轮新增的两条通道）：**

1. **inlayHint 的 label part 带 `command` 时是真的可点的** —— 普通左键、
   鼠标变 `PointingHand`、派发到 `apply_code_action` → `workspace/executeCommand`。
   于是可以做出**一行工具栏**：把 `◀ 上一章 │ 下一章 ▶ │ 📑 目录 │ 📚 书架 …`
   作为一支 inlay hint 挂在页头之后那一行空行上。
   ⚠ `inlay_hints.enabled` **默认是 `false`**（`assets/settings/default.json:828`），
   而且它是**语言级**设置（`language_settings.rs:135`），所以要写在
   `languages.TypeScript.inlay_hints.enabled`。我们的 hint 不带 `kind`，
   走 `show_other_hints`（默认 true）。
2. **codeAction 只带 `command`、不带 `edit`** 时，Zed 会去执行那条命令而不是改 buffer
   （`lsp_store.rs:6559` 起：先 resolve，再检查 `edit`，没有 edit 就取 `command`）。
   于是 `ctrl-.` 变成我们的**原生功能菜单**。

### 实现

`novel-core::ui` 现在产出三样东西，全部由同一份状态渲染，不可能互相脱节：

```rust
pub struct Rendered {
    pub text: String,          // 页面正文（可读，LSP 缺席时也能当清单读）
    pub lenses: Vec<Lens>,     // codeLens：行上方的按钮
    pub symbols: Vec<Symbol>,  // Outline 面板的条目
    pub toolbar: Option<Toolbar>, // inlayHint：一行内的按钮条
}
```

- `handy_actions(ui)` → 工具栏按钮（**短**，按页面裁剪；正文页给上一章/下一章）
- `menu_actions(ui)` → `ctrl-.` 菜单项（**全**，在工具栏基础上补「上一章/下一章」，
  保证在任何页面都能从菜单翻章）
- 工具栏的行号由 `navbar()` 取「页头之后那行空行」的下标
  （`section()` 会补空行，所以要先确保末尾空着再取下标，否则 inlay hint 会插进正文中间）

### 测试（36 passed）

新增 `crates/novel-lsp/tests/native_buttons.rs`，专门验证三条通道的**协议形状**：

- 工具栏是一支 inlay hint，挂在**空行**上、`paddingLeft` 为真、
  每个可点 part 都带完整 `Command` 对象（title + command + arguments）、
  按钮文字两侧留白、命令都在 `COMMANDS` 里注册过
- 不可见的行**不发** hint（Zed 只问可见范围）
- 点工具栏按钮真的翻页，且换章后工具栏上出现「◀ 上一章」
- 菜单项都带 `novel · ` 前缀、`kind: quickfix`、**不带 `edit`**（带 edit 就会去改 buffer）
- 三条通道声明的命令集合必须都与 `novel_core::COMMANDS` 一致 ——
  否则某个入口会点到不存在的命令

`novel-core` 侧另加两条不变量：
- 每页都有工具栏、非空、按钮无重复、标题不换行、挂在空行上、行号不越界
- 工具栏要给出**当前页最需要的**动作（正文页要有上一章/下一章；
  正文页不该有「正文」按钮；目录页不该有「目录」按钮；每页都要能去别的页）

### 仍然做不到的

- 扩展**不能**注册 dock 面板、不能开窗口、不能画自绘 UI（§13）
- Zed 的 `zed://` URL 只支持有限动作（`file`/`settings`/`extension`/`open`/`agent`/
  `schemas`/`git`），**没有「执行命令」这一条**；`zed-dock-action://<index>` 是 dock
  自己右键菜单按下标寻址，不可用（`open_listener.rs:157-205`）

---

## 16. 工具栏改为「同一行的 codeLens 组」（并删掉 inlayHint 路线）

### 起因

用户反馈「看不到工具栏」。查日志（`target/debug/novel-lsp.log`）后确认：

```
客户端 Zed · codeLens={"dynamicRegistration":true} · showDocument=false
adopt file:///F:/rust/fishtouching/.novel/reader.novel -> ... Reader 第 1 章
inlayHint -> 1 支
documentSymbol -> 7 个
codeAction -> 5 项   （×15）
didChange（81 行）→ 说明 Zed 重载了 buffer，且重新拉过 codeLens
```

**能确定的事**：Zed 确实拉起了我们的 LSP、确实请求了 inlayHint/codeAction/
documentSymbol、而且 `didChange` 证明「只写盘 → Zed 自己重载」这套架构是**成立的**。
所以管道是通的；问题在工具栏那条通道本身（inlay hint 要额外的 `inlay_hints.enabled`
开关，默认 `false`）。

**顺带一个教训**：这个日志文件被**测试和 Zed 共用**（两边都跑 `target/debug/novel-lsp.exe`），
所以 `inlayHint -> 1 支` 这类行不能当作「Zed 收到了」的证据 ——
必须看 `客户端 Zed` 那一行来区分会话。

### 改用 codeLens 组

`crates/editor/src/code_lens.rs`：

- `CODE_LENS_SEPARATOR: &str = " | "`（`:24`）
- `group_lenses_by_row(lenses, snapshot)` 按 `position.to_point().row` 分组（`:580-600`）
- 渲染时同行条目之间插入分隔符（`:631`）

→ **N 条 lens 锚在同一行 = 界面上的一排按钮。** 这就是工具栏。

`novel-core::ui::Out::toolbar(buttons)`：

1. 确保末尾是空行（否则工具栏会插进正文中间）
2. 记下 `line = lines.len()`
3. 生成那一行的文字：`// 控件  <label1>  │  <label2>  │  …`
   —— 这一行**同时是按钮的清单**，LSP 缺席时也读得懂
4. 每个按钮推一条 `Lens { line, .. }`（**全部同一行** —— 分组的前提）
   和一条 `Symbol { line, start, end }`（**行内各自的字符段**）
5. 推入那一行文字

关键细节：**符号 range 不能相同**，否则 Zed 的 Outline 会把它们挤成一个；
所以工具栏每个按钮占行内自己的一段（`Symbol` 新增 `start`/`end` 字段，
`documentSymbol` 处理器改用它们，并且仍然保证非零宽）。

### 删掉的

- `inlayHintProvider` 能力 + `textDocument/inlayHint` 处理器 + `range_lines` 助手
- `.zed/settings.json` 里的 `languages.TypeScript.inlay_hints`
- reader 页顶部那组 `export function prevChapter/nextChapter/openToc` 函数桩
  （工具栏已经在顶上，两套重复；底部那组保留并改成可读清单行）

`Toolbar` 结构体也去掉了 —— 现在只剩 `Rendered::toolbar_line: Option<u32>`，
因为「工具栏」不再是一种控件，而是「同一行上的多条 codeLens」这个事实。

### 现在的样子

```
/**
 * 《夜色渐浓》 —— 示例作者
 * 源: 演示源·站点A · 第 1 / 3 章 · 推开门 · 已启用 3 个源
 */

// 控件  下一章 ▶  │  📑 目录  │  📚 书架  │  🔍 搜索  │  ⚙ 配置     ← 这一行 5 个按钮
```

目录页则是 `// 控件  📖 正文  │  📚 书架  │  🔍 搜索  │  ⚙ 配置`。

### 测试（36 passed）

`tests/native_buttons.rs` 重写为验证 codeLens 组：

- 找出「lens 最多的那一行」当工具栏，断言 ≥4 条且那一行以 `// 控件` 开头
- 每条都在同一行、`character` 都是 0（Zed 按行分组，不按列）
- 每个按钮在那一行的清单文字里都能找到
- 每个按钮都有自己的**非零宽、互不重叠**的符号段
- 点工具栏按钮真的翻页；换章后出现「◀ 上一章」
- `ctrl-.` 菜单项不带 `edit`、命令集合与 `COMMANDS` 一致
- 逐页走一遍，每页都有工具栏

`novel-core` 侧把「工具栏」当同一行的 lens 组来断言，并检查每页
「除当前页外的四个入口都在」。

---

## 17. 「左侧小按钮」的最终答案（已查证到底）

### 用户要的东西

> 「在 Zed 的左侧面板下方添加一个小的入口，就像 Zed 的文件、Git 一样，点一个小按钮，上面显示。」

### 查证结果：**做不到。但比想象的近。**

**先定位那排按钮到底是什么**（这一条之前没人查过）：

`[源码] crates/workspace/src/workspace.rs:1974-1985`

```rust
let left_dock_buttons = cx.new(|cx| PanelButtons::new(left_dock, cx));
status_bar.update(cx, |status_bar, cx| status_bar.add_left_item(left_dock_buttons, cx));
```

**那排小按钮不是画在 dock 里的，而是状态栏（status bar）的 left items** ——
也就是**窗口左下角**最左端那一组。`Dock::render` 只画面板内容和 resize handle
（`dock.rs:1336-1385`），里面没有按钮条。所以用户说的「左侧面板下方」，
位置描述是准确的：它们就在左面板区域的下方。

- 按钮本体：`IconButton::new((name, is_active), icon)` `[源码] dock.rs:1536`
- 点击派发：未激活 → `Panel::toggle_action()`（如 `ProjectPanel::ToggleFocus`）；
  已激活 → `Dock::toggle_action()`（`workspace::ToggleLeftDock`）`[源码] dock.rs:1212-1218, 1543-1550`
- 数据来源：遍历 `Dock.panel_entries`，`Panel::icon()` 返回 `None` 就跳过
  `[源码] dock.rs:1396-1403`
- `xxx.button` 设置项控制的就是 `Panel::icon()` 返不返回 `Some`
  `[源码] project_panel.rs:8033-8037`、`terminal_panel.rs:1786-1795`
  —— **没有第二套按钮**，右键「Hide Button」是反向写回 `[源码] dock.rs:101,1450`

**为什么加不进去**（三重证据）：

1. `Panel` trait 是 `pub` 的（`dock.rs:36`），`Workspace::add_panel` 也是 `pub`
   （`workspace.rs:2828`），**但 `Dock::add_panel` 是 `pub(crate)`**（`dock.rs:629`）
   —— 只在 Zed 自己的 Rust 编译期可见。
2. wasm 侧零入口：`ExtensionManifest` 全字段（`extension_manifest.rs:84-116`）无 UI 字段；
   10 个 WIT 版本（v0.0.1–v0.8.0）grep `panel|webview|status-bar|activity` **零命中**；
   `extension_host/Cargo.toml` 根本不依赖 `gpui`/`workspace`。
3. `zed://` 全家族**没有「执行任意 action」这一条** `[源码] open_listener.rs:157-210`
   —— 只有 `file`/`ssh`/`extension`/`agent`/`skills`/`schemas`/`settings`/`git`/`open`。

**顺带排掉一个诱人的假线索**：`zed-dock-action://<index>` 里的 "dock" 是
**Windows 任务栏跳转列表**（jump list），不是面板 dock
`[源码] platform.rs:1200-1218`、`destination_list.rs:121`。
它指向 `jump_list.dock_menus`，而那里只有一个 `MenuItem::action("New Window")`
`[源码] history_manager.rs:101-109`。跟面板毫无关系。

**唯一能真加按钮的路**：fork Zed，在 `initialize_panels()`（`zed.rs:776`）里
注册自己的 `Panel`（实现 `icon` / `icon_tooltip` / `toggle_action` /
`activation_priority`）。代价：维护 fork、跟上游 rebase、无法通过扩展市场分发。

### 最接近的替代（已实现）

**关键洞察：虽然加不了新按钮，但可以让 Zed 已有的某个按钮打开我们的内容。**

终端面板的按钮 `terminal.button` 默认就是 `true`，而
`TerminalPanel::position()` 读的是 `terminal.dock` 设置、`position_is_valid()` 恒 `true`
`[源码] terminal_panel.rs:1682-1688`。所以：

```jsonc
"terminal": { "dock": "left", "button": true }
```

→ **终端的小按钮就出现在窗口左下角那排按钮里，和文件/Git 并排**（位置正是用户要的）。

再跑一次任务 `novel: 面板`（`reveal:"always"` + `hide:"never"` +
`reveal_target:"dock"` + `use_new_terminal:true`），往那个左侧面板里打印

```
novel ──────────────────────────────────────────────────────────
《夜色渐浓》 —— 示例作者
源 演示源·站点A · 3 章 · 已启用 3 / 3 个源
正在读 第 1 / 3 章 · 推开门

书架
▶ #0 夜色渐浓   示例作者     3 章
  #1 长夜将尽   另一个作者   4 章
  #2 雨落长安   第三作者     4 章

目录（共 3 章）
▶   1. 推开门
    2. 风灌了进来
    3. 稿纸哗哗作响

书源
☑ 演示源·站点A  →  novel source 0
☐ 演示源·站点B  →  novel source 1
☑ 演示源·站点C  →  novel source 2

搜索（还没有关键字）
  novel search 书名

命令 ───────────────────────────────────────────────────────────
  novel shelf / toc / read / config   切页面
  novel next / prev                  翻章
  novel goto N                       跳章
  novel open N                       换书（第 N 本）
  novel source N                     开关书源
  novel search 书名                   搜索
  novel panel                        重印这份面板
```

之后点那个小按钮、或按 `ctrl-`` （`terminal_panel::Toggle`，默认已绑）就能开合它。

**这是纯文本、不是 TUI**（用户明确拒绝 TUI）：`novel-lsp panel` 打印一次就退出，
终端保持在那里。好处是原位滚动/复制/搜索全用 Zed 原生终端能力，零新依赖、零渲染风险。
刷新方式是再跑一次任务或 `novel panel`。

### 落地清单

| 文件 | 改动 |
|---|---|
| `crates/novel-core/src/textwidth.rs` | **新增**：`is_wide`/`width`/`pad`/`truncate`（按显示宽度对齐，中文占 2 列）；`probe.rs` 的私有副本删掉改用这份 |
| `crates/novel-core/src/ui.rs` | **新增** `ui::panel(&UiState) -> Vec<String>`：纯文本仪表盘 + 3 条测试 |
| `crates/novel-lsp/src/main.rs` | 新增 `panel`（只读，不写文件）和 `source N`（开关书源）子命令 |
| `.zed/settings.json` | `"terminal": { "dock": "left", "button": true }` + 注释说明为什么 |
| `.zed/tasks.json` | 新增任务 `novel: 面板`（需要终端，所以走 `reveal:always` 那条规则） |
| `.zed/keymap.json` | `ctrl-alt-n l` → `novel: 面板` |
| `crates/novel-lsp/tests/zed_config.rs` | `NEEDS_A_TERMINAL` 常量扩成两个任务 |

`panel` 做成**只读**是刻意的：它能被反复跑（刷新），而不会把阅读页顶掉。

---

## 18. P0.3：交互搬进面板，阅读页只负责渲染

### 用户的要求

> 「可以进行交互了，交互放到面板里面，打开的部分只负责渲染随机的 TS 代码和通过注释显示加载到的小说。」

### 拆开看是两个改动

**① 交互全部搬到左下角的终端面板。**
面板不再是「打印一次的只读视图」，而是**主入口**：搜索、选书、选章节、开关书源都在里面。

实现方式是**「打印一屏 → 读一行 → 改状态 → 重画一屏」**，而不是全屏 TUI：

* 终端自己提供行编辑、历史（上箭头）、复制粘贴；
* 不需要 raw mode，也就不需要 `crossterm` —— **零新依赖**；
* 每次操作只重绘一次，不是动画，绕开了 Zed 在 Windows 上 TUI 残影那个坑（#52184）。

（`probe.rs` 保留着，用来随时验证「全屏 TUI 到底行不行」，但当前面板不依赖它的结论。）

**② 阅读页不再承担任何交互。**
它只做两件事：渲染随机 TS 代码 + 把小说放进 `//` 注释。
所以 codeLens / inlayHint / codeAction / documentSymbol **全部删掉** ——
那个「五页面 + 工具栏」的方案整块下线了。

### 状态从阅读页搬到了独立 JSON

原来状态藏在阅读页末尾的 `export const state: UiState = {...}` 里，靠解析那行恢复。
现在阅读页要「只负责渲染」，就不该再兼任数据库，所以拆出来：

```
.novel/state.json     {"book":0,"chapter":2,"query":"","enabled":[true,true,true]}
.novel/reader.novel   纯渲染结果
```

于是「面板改状态」→「阅读页渲染状态」是干净的单向关系。
副作用是阅读页的渲染不再需要往返解析，`render()` 变成纯函数。

### 随机 TS 代码生成器（`crates/novel-core/src/render.rs`）

用户要「随机」，但**必须确定性** —— 否则每次写文件内容都不一样，编辑器会一直重载，
测试也没法断言。所以：

* 种子 = FNV-1a(书名, 章节标题, 章号)，走 `rng.rs` 里的 xorshift64\*；
* 八种代码块模板（interface / as const / function / 箭头函数 / async function /
  class / enum），随机挑，随机挑标识符；
* **不连着出两块同形状的**（否则会看到两个几乎相同的 class，一眼假）。

生成器要守住的不变量（都有测试）：
正文只出现在 `//` 注释里、每个声明名唯一（重复声明 TS 直接编译不过）、
花括号配平、同章逐字节一致、`pad`/`truncate` 不越界。

写测试时当场抓到两个「一眼假」的 bug：
`normalizeclampToken`（两个词硬拼）和两个相邻的 `*Cursor` 类。

### 语言服务器缩到最小

只剩「打开阅读页时，如果内容和状态对不上就写一遍」。`initialize` 里
**一个 provider 都不声明**（测试断言了这一点）。README 里说明了为什么：
它不再是「提供按钮的通道」，而是「把生成的文件物化出来」。

### 文件变动

| 变动 | 说明 |
|---|---|
| 删 `novel-core/src/ui.rs` | 五个页面 + 工具栏 + codeLens/符号锚点，整块下线 |
| 删 `novel-lsp/tests/native_buttons.rs` | 没有按钮了 |
| 删 `novel-lsp/tests/lsp_session.rs` | 被 `tests/e2e.rs` 取代 |
| 新 `novel-core/src/rng.rs` | 确定性伪随机 |
| 新 `novel-core/src/state.rs` | 状态 + 阅读页写盘（`Session`） |
| 新 `novel-core/src/panel.rs` | 仪表盘 + 输入解析（`Action`） |
| 重写 `novel-core/src/render.rs` | 随机 TS 生成器 |
| 重写 `novel-lsp/src/main.rs` | 三身份：语言服务器 / 交互面板 / 一次性命令 |
| 重写 `novel-lsp/src/lsp.rs` | 只负责渲染 |
| 新 `novel-lsp/tests/e2e.rs` | 命令行 + 面板（喂 stdin）+ 语言服务器 |
| 改 `.zed/*` | 任务表/键位/设置跟着简化（去掉 `code_lens`、`document_symbols`、`outline_panel`）|

### 测试（42 passed）

面板的测试是**把按键喂给真实子进程**：

```rust
printf 'n\ng 3\n/长夜\n2\nx 1\nq\n' | novel-lsp ui --dir <tmp>
```

然后断言最终状态、阅读页内容、以及面板上显示过什么。
另外还覆盖：烂输入不崩不中断（之后仍能翻章）、EOF 干净退出、
越界命令以 exit 2 退出且不动任何文件。

### 顺带修掉的一个 off-by-one

目录的「只列当前章附近 12 章」算式原来在渲染和测试里各写一遍，
而且**两处都把最后一章漏掉了**（`end = start + 窗口 - 1`，要包含 `total`
就得 `start = total - 窗口 + 1`）。抽成 `toc_window()` 一处实现，测试直接打这个函数。

---

## 19. 终止 Zed 整合，改做独立 TUI（v1.0）

### 为什么终止

§13/§17 已经查到底：**扩展加不了任何按钮、面板、窗口** ——
`Dock::add_panel` 是 `pub(crate)`，`ExtensionManifest` 零 UI 字段，
`zed://` 没有「执行命令」这一条。所有替代方案（借终端面板的按钮、
Outline 当归导航、codeLens 当按钮）都是**绕着限制走**，用户明确表示不要。

于是要求变成：**放弃与 Zed 整合，做一个独立的 TUI。**

### 删掉的

| 删除 | 原因 |
|---|---|
| `crates/novel-lsp/`（整个 crate） | 语言服务器只为 Zed 而存在 |
| `zed-extension/` | 不再需要声明 `novel` 语言 |
| `.zed/`（settings/tasks/keymap） | 不再需要任务与键位接线 |
| `novel-core/src/panel.rs` | 旧「打印一屏 + 读一行」面板，被 TUI 取代 |
| `novel-core/src/textwidth.rs` | 只有旧面板和 LSP 用；TUI 由 ratatui 自己算宽度 |

`novel-core` 保留 `demo` / `render` / `rng` / `state` 四个模块 —— 全部与界面无关。

### 新增 `crates/novel-tui`

依赖只有 `novel-core` + `ratatui`（默认 feature 自带并再导出 crossterm，不单列）。

分成 lib + bin 两层，**逻辑能脱离终端测**：

| 文件 | 职责 |
|---|---|
| `src/app.rs` | 纯状态机：`Key` / `Focus` / `InputMode` / `App`。不碰终端，单测直接调 `handle(Key)` |
| `src/ui.rs` | 绘制，只读 `App`；外加 `render_to_text()` —— 内存后端(TestBackend)画一遍返回文本 |
| `src/main.rs` | crossterm 事件循环 + `translate()` 把 `KeyEvent` 转成自定义 `Key` |
| `examples/preview.rs` | 无终端环境下把界面打成文本（截图/对比用） |

**自定义 `Key` 枚举是关键**：crossterm 的 `KeyEvent` 带一堆我们不要的状态，
换掉之后 `app.rs` 的测试不需要构造任何终端对象。

### 界面（四个面板）

```
 《夜色渐浓》 示例作者  第 1 / 3 章  书源 3/3
┌ 书架 ──────────────┐┌ 目录 ────────────────────────────────┐
│ ▶ 夜色渐浓  示例作者 ││ ▶    1. 推开门                      │
│   长夜将尽  另一个作者│└──────────────────────────────────────┘
│   雨落长安  第三作者 │┌ 正文预览 ─────────────────────────────┐
└─────────────────────┘│/**                                     │
┌ 书源（配置）────────┐│ * 《夜色渐浓》 —— 示例作者               │
│ 1 ☑ 演示源·站点A    ││// 他推开门，楼道里的声控灯亮了一下…    │
└─────────────────────┘└──────────────────────────────────────┘
 ↑↓ 选书  Enter 打开  1-9 开关书源  / 搜索   Tab 切面板  ? 帮助  q 退出
```

左列 = **选什么**（书架 / 搜索结果、书源开关），右列 = **读什么**（目录、正文）。

正文着色是这个界面的重点：**小说正文最亮**（`White`），生成的代码压暗一档
（`Gray`），结构注释与分隔线最暗（`DarkGray`）—— 一眼就能从「代码」里认出「正文」。

### 搜索改成边打边筛

原来（旧面板）是「输入 → 回车 → 出结果」。现在 `handle_input` 里每敲一个字符
就重算结果：打完还得再敲回车的搜索框很恼人。

只有 `Enter` 才把 `query` 写进 `state.json` —— 边打边写盘是没必要的 I/O。

### 四个实现期踩到的坑

1. **`TestBackend` 里宽字符占两格**，第二格是 `" "`（不是空串）。
   逐格拼文本会把每个汉字之间插一个空格，于是所有界面断言都失败。
   判据只能是「这个字符显示宽度是 2 吗」→ 手写 `is_wide` 区间表，
   **不为一个测试助手引 `unicode-width`**。
2. **浮层必须 `Clear`**。只给 `Paragraph` 的 `Block` 设背景色不够 ——
   底下面板的文字会从没被写到的格子里透上来。要先 `render_widget(Clear, popup)`。
3. **goto 不能预填当前章号**。预填了用户打字就变成追加（想输 12 得到 112）。
   当前章号改显示在提示语里（`跳到第几章（当前 1 / 3）`）。
4. `self.state()` 返回 `&State`，调不了 `step`/`goto` 这类 `&mut self` 方法 →
   补一个 `state_mut()`，别到处绕借用。

### 测试（53 passed）

```
novel-core   19 passed
novel-tui    34 passed   （app 20 + ui 9 + main 5）
```

`app.rs` 的测试全部是「喂键序列 → 断言状态」；`ui.rs` 的测试用
`render_to_text()` 断言「界面上确实出现了这几个词」，包括
**帮助浮层是不透明的**（透明就会被断言抓到 —— 这个断言当场抓出了漏 `Clear` 的 bug）。

### 还没验证的

**真实终端里跑一遍。** 单测覆盖不了的是 crossterm 的 key event 翻译
（`translate()`）和终端的 raw mode 进出 —— 这两块只能人工跑。

---

## 20. P1：真书源引擎（已实现，实测能读真站点）

### 目标

吃 legado 的导出格式，**不转换**，直接读现成的书源集合。

### 调研：先把方言搞准

规则方言不能靠猜，所以逐行读了参考实现，拿到了精确语义：

| 文件 | 拿到的结论 |
|---|---|
| `RuleAnalyzer.kt` | `&&`/`||`/`%%` 的切分算法：跳 `[]`/`()` 平衡组，引号只在组内生效，`\` 只在组内转义 |
| `AnalyzeByJSoup.kt` | 默认方言的步骤语义（`tag.`/`class.`/`id.`/`text.`/`children.`）、两种索引写法（`.0:1` 与 `[0,-1,3:5]`）、取值器的确切行为 |
| `AnalyzeRule.kt` | 前缀识别顺序、`##` 的三/四段语义、`{{ }}` 的双重身份（URL 里一律当 JS，规则里先看像不像规则） |
| `AnalyzeUrl.kt` | `,{json}` 选项的全部字段、`\s*,\s*(?=\{)` 切分、三个超时、`enabledCookieJar` 三态 |
| `BookChapterList.kt` / `BookList.kt` | **子规则的求值起点是列表里的那个元素**，不是整篇文档 |

### 实测出来的、文档里没有的东西

调研给了骨架，但真正的坑是打真站点打出来的：

1. **`@css:` 规则整段是一个选择器**，`@` 只出现一次分隔取值器，不参与逐级收窄。
   而默认方言里 `@` 是「每一级」的分隔符。同一个字符两种含义。
2. **真书源的 URL 选项用单引号**（`{'charset':'gbk',…}`）—— 123 个源这么写。
   GSON 的宽松模式认，`serde_json` 不认。要兜一下。
3. **`searchUrl` 常写成相对路径**（`/tag/?key={{key}}`），
   基准是 `bookSourceUrl`，而且是**重定向之后**的那个（`ddyueshu.com` 已经 301 到 `ddyueshv.cc`）。
4. **`{key}` 是死写法**。legado 自己也不替换它（只有 `{{key}}`，那是 JS 变量）。
   18 个源这么写，都是历史遗留。
5. **正文要先洗 HTML 再反转义**。反了就完蛋：`&lt;div&gt;` 先变成真标签，
   然后被当成标签删掉。legado 的 `getString(unescape = false)` 就是为这个存在的。
6. **`HtmlFormatter` 的空白判定是 ASCII 的**。Rust 的 `\s` 默认是 Unicode 空白，
   会把中文排版的全角空格 U+3000 一起吃掉，Java 不会 —— 必须写成显式集合。

### 依赖选择

| 需求 | 选择 | 为什么不是别的 |
|---|---|---|
| HTTP | `ureq`（39 个传递依赖，rustls） | `reqwest` 要拖 tokio |
| HTML 解析 | `tl`（**零**传递依赖） | `scraper` 要多带 60 个 |
| HTML 选择器 | **自己写** | `tl` 的 `query_selector` 不支持组合符（`div a` 返回 0 个），真书源全是组合符；而且真书源大量用 JSoup 私有扩展（`:eq`/`:contains`/`:matches`/`:has`），任何标准 CSS 库都没有 |
| 编码 | `encoding_rs`（零传递依赖） | |
| 正则 | `regex` | |
| JSON 路径 / XPath | **不做** | 只解锁 25 + 1 个源，见下面的统计 |

### 覆盖率：数据说话

拿 3907 个真实书源（21 MB）统计：

```
启用的文本源           3876
不能搜索                698
本实现能跑             1178   ← 规则里不含 JS / XPath / JSON 路径
按域名去重              596
实测 30 个源走通全链路      3   ← 站点都死了，不是引擎的问题
```

阻塞项（一个源可能有多个）：`@js:` 1613、URL 里的真 JS 表达式 987、
JSON 路径 658、XPath 95、正则 34。

**结论：要覆盖整个生态，JS 解释器是必需品，不是可选优化。**
而 JSON 路径只解锁 25 个源（那 658 个同时被 JS 挡着），所以现在不做。

### 分页上限（三条改进里的第 1 条，已做）

legado 的目录/正文分页只靠 URL 去重，站点返回递增 URL 就会无限抓下去。
本实现 `MAX_PAGES = 50`，且每个 `nextUrl` 都要和已访问集合比一次。

### 测试

**122 passed**，其中最有价值的是语料测试：把 3907 个书源里 15276 条规则串
全部过一遍（切分 → 解析 → 求值），只要求不崩。它抓到两个手写测试
永远覆盖不到的崩溃：

1. `{{$.chapter_id}}` 让词法分析器原地打转，每轮塞一个 token 直到申请 64 GB 内存；
2. `starts_ci` 写成 `s[..n]`，`text.下一页@href` 的第 6 个字节落在汉字中间 → panic。

两个都是「拿真数据喂一遍」才会现形的那类 bug。

另外 6 条 `--ignored` 的测试真打站点，覆盖到「搜索 → 目录 → 正文 →
落在阅读页的 `//` 注释里」这整条缝。

---

## 21. P1.5：内置 JS 解释器（QuickJS）

### 为什么必须做

P1 落地后拿 3907 个真源统计，**1613 个用 `@js:`、987 个在 URL 里用真 JS 表达式**。
不支持 JS 就只能覆盖三成。这次做完，能被覆盖的源从 1178 涨到 **1453**，
而且解锁的是**完全不同的一批**（343 个去重域名，之前只会报 `⊘ 需要 JS`）。

### 引擎选型：实测数据说话

| | `boa_engine` | **`rquickjs`** |
|---|---|---|
| 依赖（唯一） | 124（`icu_*` 一大坨）| **21** |
| 冷编译 | 105 s | **35 s** |
| release 增量 | 未测（icu 太重）| **1.30 MB** |
| 需要 | 纯 Rust | 一个 C 编译器 |

选 QuickJS。**并且不开 `bindgen` feature** —— 那会额外要求 libclang，
而 `rquickjs-sys` 自带预生成的 `quickjs.bind.h`，有一个 C 编译器就够。

### 架构：Rust 只暴露原语，legado 的 API 形状用 JS 写

这是这一节最值钱的决定。

一开始想在 Rust 里直接把 `java.md5Encode`、JSoup 的 `Elements` 一个个暴露成 JS 函数。
撞了三堵墙：

1. `Function::new(ctx, f)` 要求 `f: Fn(参数) -> 返回` 且参数实现 `FromParam`，
   一旦要 `Ctx` / `Value` / 变长参数就和 `'js` 生命周期打架；
2. `Object<'js>` 对 `'js` 是**不变**的，`Function<'_>` 传进辅助闭包立刻报
   「borrowed data escapes」；
3. 真书源的元素对象有 `length` / `size()` / `get(i)` / `select()` / `attr()` /
   `text()` / `html()` / `first()` / `toArray()` / `toString()` 十几个方法，
   在 Rust 里逐个写要处理借用，300 行打不住。

改成：Rust 只装十几个签名极简的原语（`__ajax(url)`、`__rule(rule, mode)`、
`__sel(html, sel)`、`__md5(s)` …），全部是 `Fn(String) -> String` 这种；
然后在 **200 行 JS prelude** 里把 legado 的形状拼出来。
`Function` 的签名问题、`Element` 的十几个方法、Java 类互操作，全在 JS 里解决。

顺带在 prelude 里用 `Proxy` 包了 `java`：**没实现的 Java 类会报出它自己的名字**
（`java.aesBase64DecodeToString 未实现（本实现不做 JVM 类互操作）`），
而不是让 JS 报「cannot read property of undefined」——
这个项目里「某个书源用了没实现的 Java API」是最常见的失败，
报清楚名字能省掉一轮排查。

### 踩到的坑

1. **QuickJS 的 `Error::Exception` 不带原因**。`Display` 出来永远是一句
   「Exception generated by QuickJS」。真正的信息在挂起的异常对象上，
   必须用 `.catch(ctx)` 取。少了这一步，所有 JS 报错长的都一样。
2. **`set_max_stack_size` 比的是原生栈用量**。我一开始设 2 MB，而测试线程栈也就
   2 MB，于是 JS 无限递归把进程带崩（`STATUS_STACK_OVERFLOW`）而不是给一个可
   捕获的错误。改成 1 MB，并要求 worker 线程栈 ≥ 8 MB。
3. **strict mode 下「只读属性 + 赋值」会抛 `no setter for property`**。
   我在 prelude 里先用 `defineProperty` 把 `size` 定义成 getter，
   又用 `=` 赋值，prelude 直接装不上。按真数据统一成方法（`size()` 289 次）
4. **`result` 的初始值**。legado 的管线是 `result = content` 起步的，
   所以**裸 `@js:` 脚本拿到的 `result` 是整页 HTML**
   （`@js:result.match(/id="(\d+)"/)[1]` 这种写法非常常见）。
   我一开始喂空串，一批脚本全废。

### `{{ }}` 里的三种东西

legado 的 `isRule()` 规定：`{{ }}` 里以 `@` / `$.` / `$[` / `//` 开头是**规则**，
否则才是 JS。实测分布：

```
{{$.…}} JSON 规则   3888
{{…}} 当 JS          949
{{@@…}} 规则         641
{{@其它}}             67
{{@css:…}} / {{/…}}    6
```

所以 `expand_braces` 是**三档**：内联算术（`{{page}}` 五万多次，不该绕 JS）
→ 内嵌规则（`{{@@class.x@text}}`，回调进规则求值器）
→ JS。

### 测试

**137 passed**（JS 部分 +15）。最有价值的是 JS 语料测试：把语料里的
**1657 条真 JS 片段**全部喂进 QuickJS，实测 **1146 条跑通（69%）**。
剩下 31% 里真正是 API 缺口的只是少数（AES 之类），
大部分是测试用的是假 `result`，所以 `result.match(...)[1]` 取不到 ——
真跑时 `result` 是真页面内容。

真打站点也验证过：`sources-js.json` 里 24 个源有 2 个走通全链路
（笔趣阁③ 1663 章、零零小说 1651 章），`.novel/sources.json` 里 5 个源
**全部走通**。

### 顺带修的一个工具链坑

语料文件原本用 `\n` 分隔。加了 JS 之后这就不行了：`@js:` 脚本几乎都含换行，
用 `\n` 切会把一条规则切成好几条，JS 那边全成语法错误。
改成 `\r` 分隔（真规则里不含 `\r`），并顺手把源码统一成 LF
—— 混用 CRLF 让「按内容做替换」一直静默失配（我在这上面浪费了三次）。

---

## 22. P0.5：书架 / 章节记忆 / 书源编辑 / 自测交互

### 五件事，按「先能自测」的顺序做

用户一次提了五件事（自测交互、书源编辑、书架管理、章节记忆、切换章节快捷键）。
顺序是刻意的：**先把事件循环变成可脚本驱动的，再往上加功能** ——
否则每加一个功能都还是只能靠人眼验，回归一次要手工敲一遍。

### ① 自测交互：把事件循环从 `main()` 里挖出来

原来「按键 → 重画 → 再按键」长在 `main()` 里，是唯一一块只能人眼验的地方。
现在拆成 `run.rs`：

```rust
pub fn run<D, E>(app: &mut App, workers: &Workers, draw: D, next: E) -> io::Result<()>
where D: FnMut(&mut App) -> io::Result<()>,
      E: FnMut(&mut App) -> io::Result<Option<Key>>,
```

真跑时 `next` 是 crossterm；测试和 `--script` 时是 `Driver` ——
一个按键 + 后台消息**交替**喂的驱动器：

```rust
Driver::new(vec![Step::Key(Key::Enter)], tx)
    .feed(Done::Toc(toc()))          // 模拟「目录回来了」
    .feed(read("第一章", "正文"))     // 模拟「正文回来了」
    .key(Key::Char('n'))
```

交替是必须的：抓取是异步的，「按回车 → 排任务 → 结果回来 → 重画」中间隔着
好几次循环。只喂按键的话，测试会在结果回来之前就跑完了。

`--script` 给用户同一套能力：`novel-tui --script '{wait:8000}{down}{enter}'`。
`{wait}` 是必需的 —— 在线脚本不等的话，会在结果到达前把后面的键全按完。

### ② 书架 + 章节记忆：`shelf.json`

**和 `state.json` 分开**：一个是「这一刻光标在哪」，一个是几个月的阅读进度，
生命周期完全不一样，混在一起的话一次状态文件损坏就连带把进度弄丢。

几条刻意的决定：

* **打开过的书自动进书架**。让人再按一次收藏属于无谓的税。
* **每读一章就记一次**（章号 + 标题 + 时间），书架按最近阅读倒序。
* **按 URL 认书，不按书源下标**。书源列表会因为导入/删除重排，
  用下标当主键的话删一个源就会把别人的进度串到另一本书上 —— 这条有专门的测试。
* **重新收藏不冲掉进度**：元信息更新，章节保留（进度是资产）。

### ③ 书源编辑：把「启用状态」的第二份真相删掉

`state.json` 里原本有一份 `enabled: Vec<bool>`，和 `BookSource.enabled` 两份并存。
一旦要把书源写回文件，这就是 bug 工厂（「界面显示开着、实际用的是关的」，
只在特定操作顺序下复现）。所以直接把 `State.enabled` 整个删掉，
开关只落在 `BookSource.enabled` 上，改完立刻写回 `sources.json`。

编辑只开放三个字段：**名字 / 网址 / 搜索网址**。真书源失效十有八九是换了域名，
所以后两个比名字有用得多。编辑框**预填当前值**（和 `g` 跳章相反）——
改域名只动几个字，从零敲累得多。

导入接受**文件路径或 URL**，按 `bookSourceUrl` 去重（同一份集合导入两次不该变两份）。

### ④ 章节快捷键

`n`/`p` 之外加了 `]`/`[`（同一功能的别名：右手在字母区用前者，
左手在方括号用后者）、`.`/`,`（末章/首章）、`}`/`{`（书架换书）。

### 踩到的坑（这一轮一大半是「测试台自己的 bug」）

自测的价值在这一轮体现得特别直白 —— 五条 bug 里有三条是**测试台的**，
而它们伪装成「功能坏了」：

1. **驱动台没给退出键** → `run` 只在 `app.quit` 时返回 → 死循环。
   修在驱动台里（跑完自动设 `quit`），不是让每个测试自己记得补一个 `q`。
2. **收尾发假的 `q`/`Esc` 键会污染断言** —— `Esc` 会清掉状态栏，
   于是「删掉了书源」这条断言再也看不见。改成**直接设 `app.quit`**，不按键。
3. **`app.quit` 泄漏到下一次 `run`**：同一个 `App` 会跑好几轮（改一下、再跑一轮），
   上一轮设的 `quit` 会让第二轮的第一次循环立刻返回 ——
   表现是「脚本只执行了第一个键」，看起来像功能坏了。修法是 `run` 入口清一次。
4. **书架路径把 `.novel/` 算了两遍**（真 bug）：写成「state 的目录 + `.novel/shelf.json`」，
   在真程序里凑巧对（state 路径本来就带 `.novel/`），
   但配合 `--dir` 和测试用的临时目录就写到一个谁也不看的地方去。
   而且测试之间会**通过真实文件系统互相污染**（`crates/novel-tui/.novel/shelf.json`
   就是这么冒出来的，那条测试于是读到上一个测试留下的书架）。
5. **从搜索结果打开的书，书架上记的「书源名」记成了书名**（真 bug）：
   下次按书源名反查就找不到（「书源「斗破苍穹」不在了」）。

还有一个不是 bug 但值得一提：**驱动台的「还要等吗」判断漏了「有待办任务」**。
`Done::Toc` 处理完会立刻排一个 `Job::Read`，而 `busy` 要到下一轮 `pump` 才置上，
只看 `busy` 会在这个缝里判成空闲、提前收尾 —— 表现是「正文永远出不来」。
`App::has_pending_job()` 就是为这个加的。

另外顺手把 `parse_script` 补上控制字符映射（`\u{15}` → `Ctrl-U`）——
真终端里 Ctrl-X 送的就是这些字节，脚本里也该照收。

### 测试

**172 passed**（+35），其中 `tests/interaction.rs` 17 条跑的是真实事件循环。

真打站点也验过整条链：

```
$ novel-tui --script $'/斗破苍穹\n{wait:9000}{down}{enter}{wait:5000}'
 《斗破苍穹.2》 柴老五  第 1 / 327 章  书源 5/5        正

$ head -4 .novel/reader.novel
/**
 * 《斗破苍穹.2》 —— 柴老五
 * 第 1 / 327 章 · 正文 正文_第1章：结识神秘老者
 * 源 鬼吹灯

$ novel-tui --script '{enter}{wait:6000}nn{wait:15000}'   # 翻到第 3 章
$ novel-tui --script '{enter}{wait:8000}'                 # 模拟重启
 目录 327 章，接着读第 3 章                                ← 章节记忆生效
```
