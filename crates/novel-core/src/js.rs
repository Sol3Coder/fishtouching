//! JS 执行环境 + legado 的 `java.*` 宿主 API。
//!
//! # 引擎选型
//!
//! 用 QuickJS（`rquickjs`），不是纯 Rust 的 `boa_engine`：依赖差 6 倍
//! （boa 124 个，`icu_*` 那一大坨；rquickjs 21 个），release 只多 1.3 MB。
//! 代价是需要一个 C 编译器（`rquickjs-sys` 自己编 vendored QuickJS）。
//! **不开 `bindgen` feature** —— 那会额外要求 libclang，而预生成的绑定就够。
//!
//! # 两层结构（这一层最重要的设计决定）
//!
//! Rust 侧**只暴露一组签名极简的原语**（`__ajax(url)`、`__rule(rule, mode)` …），
//! legado 那套 `java.md5Encode` / `Java Elements` 的形状全部写在 [`PRELUDE`] 这段
//! JS 里。原因：
//!
//! * Rust 闭包直接暴露成 JS 函数要满足 `Fn(参数) -> 返回` 且参数类型实现
//!   `FromParam`，一旦要 `Ctx` / `Value` / 变长参数就会和生命周期打架；
//! * 真书源的元素对象有 `length` / `size()` / `get(i)` / `select()` / `toArray()`
//!   一大堆方法，在 JS 里写是 30 行，在 Rust 里写是 300 行且每行都要处理借用。
//!
//! # 线程约束
//!
//! `Runtime`/`Context` 都不是 `Send`，所以 `Js` 只能**用完即弃**地在需要它的
//! 那个线程里建。`online.rs` 的 worker 线程每次任务建一个。

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use rquickjs::{Context, Function, Object, Runtime};

use crate::{Error, R};

/// JS 里能调到的宿主能力。规则引擎实现它 ——
/// 这一层刻意不认识 HTML 和网络，那些通过这个 trait 注入。
///
/// 全部返回「失败就给空值」而不是抛异常，和 legado 一致：
/// 一个源里 `java.getString("...")` 取不到东西是常态，不该让整段脚本炸掉。
pub trait Host {
    /// `java.ajax(url)`。返回正文；失败给空串。
    fn ajax(&self, url: &str) -> String;

    /// `java.setContent(html)`：换掉后续 `java.getString` 的求值对象。
    fn set_content(&self, html: &str);

    /// `java.getString(rule)`：把 legado 规则在**当前内容**上求值。
    ///
    /// `mode`：`"s"` 取一个字符串、`"l"` 取列表、`"o"` 取元素的 outerHTML。
    /// 列表和元素都按 `\n` 接起来给 JS。
    fn rule(&self, rule: &str, mode: Mode) -> String;

    /// `java.put(k, v)` / `java.get(k)`：书源变量。
    fn put(&self, key: &str, value: &str);
    fn get(&self, key: &str) -> String;

    /// 在给定的一段 HTML 上做一次选择 / 取属性 / 取文本。
    /// 元素包装对象的 `select` / `attr` / `text` / `html` 走这里。
    /// 返回值同样按 `\n` 分隔。
    fn html_select(&self, html: &str, selector: &str) -> String;
    fn html_text(&self, html: &str) -> String;
    fn html_attr(&self, html: &str, name: &str) -> String;
    fn html_remove(&self, html: &str, selector: &str) -> String;
}

/// `java.getString` 家族要的返回形态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// 一个字符串（多个结果用 `\n` 接）
    Text,
    /// 列表（用 `\n` 接，JS 侧再 split）
    List,
    /// 元素的 outerHTML（多个用 `\n` 接）
    Html,
}

/// 脚本能直接看到的全局变量。
#[derive(Debug, Default, Clone)]
pub struct Bindings {
    pub page: u32,
    pub key: String,
    pub base_url: String,
    pub source_url: String,
    pub source_name: String,
    pub source_comment: String,
    pub book_name: String,
    pub book_author: String,
    /// 当前正文（legado 叫 `src`）
    pub src: String,
    /// 上一次规则求值的结果（legado 的 `result`）
    pub result: String,
    /// 正在拼的那一章。真书源里 `@js:` 常写 `chapter.title`。
    pub chapter_title: String,
    pub chapter_url: String,
    pub chapter_index: usize,
}

/// 脚本的返回值。legado 会按类型区别对待，所以不能一律转字符串。
#[derive(Debug, Clone, PartialEq)]
pub enum Out {
    Str(String),
    Num(f64),
    Bool(bool),
    /// JS 数组
    List(Vec<String>),
    /// `null` / `undefined`
    Nothing,
}

impl Out {
    /// legado 的强转规则：字符串原样；整数双精度去掉小数点；其余 `toString()`。
    pub fn to_text(self) -> String {
        match self {
            Out::Str(s) => s,
            Out::Num(n) if n.fract() == 0.0 && n.abs() < 1e15 => format!("{n:.0}"),
            Out::Num(n) => n.to_string(),
            Out::Bool(b) => b.to_string(),
            Out::List(v) => v.join("\n"),
            Out::Nothing => String::new(),
        }
    }

    pub fn to_list(self) -> Vec<String> {
        match self {
            Out::List(v) => v,
            Out::Nothing => Vec::new(),
            other => vec![other.to_text()],
        }
    }
}

/// 一个 JS 执行环境。
pub struct Js {
    /// 只是为了持有它。`Context` 内部是引用计数的，所以两个字段的释放顺序无所谓。
    _rt: Runtime,
    ctx: Context,
    /// 上一次 `java.put` 的变量落在哪 —— 退出时由调用方取回，写回书源变量表。
    pub vars: Rc<RefCell<HashMap<String, String>>>,
}

impl Js {
    pub fn new(host: Rc<dyn Host>, b: &Bindings) -> R<Self> {
        let rt = Runtime::new().map_err(|e| Error::Js(e.to_string()))?;
        // 限制堆和「JS 栈」。
        //
        // `set_max_stack_size` 是 QuickJS 用来判栈溢出的阈值，它比的是**原生栈用量**。
        // 设得和线程栈一样大就等于没设：JS 无限递归会把进程直接带崩
        // （STATUS_STACK_OVERFLOW），而不是给一个能捕获的错误。
        // 所以这里保守取 1 MB，并且**调用方要保证线程栈更大**（见 online.rs）。
        rt.set_memory_limit(64 * 1024 * 1024);
        rt.set_max_stack_size(1024 * 1024);
        let ctx = Context::full(&rt).map_err(|e| Error::Js(e.to_string()))?;
        let vars = Rc::new(RefCell::new(HashMap::new()));
        let me = Self { _rt: rt, ctx, vars };
        me.install(host, b)?;
        Ok(me)
    }

    /// 求值一段 JS。`result` 是 legado 的 `result` 绑定。
    pub fn eval(&self, src: &str, result: Option<&str>) -> R<Out> {
        self.ctx.with(|ctx| {
            // **每次都重设**：legado 的 bindings 是 per-call 的。
            // 只在 Some 时设置的话，上一次的 result 会漏到下一次。
            match result {
                Some(r) => {
                    let _ = ctx.globals().set("result", r);
                }
                None => {
                    let _ = ctx
                        .globals()
                        .set("result", rquickjs::Value::new_undefined(ctx.clone()));
                }
            }
            match eval_in(&ctx, src) {
                Ok(v) => Ok(to_out(&v)),
                Err(why) => Err(Error::Js(format!("{why} | 脚本：{}", clip(src)))),
            }
        })
    }

    fn install(&self, host: Rc<dyn Host>, b: &Bindings) -> R<()> {
        let vars = self.vars.clone();
        self.ctx.with(|ctx| {
            let jerr = |e: rquickjs::Error| Error::Js(e.to_string());
            let g = ctx.globals();

            // 用宏而不是闭包来装原语：`Function<'_>` 传进辅助闭包时，
            // `Object` 对 `'js` 是不变的，借用检查过不去。
            macro_rules! prim {
                ($name:literal, $f:expr) => {{
                    let f = Function::new(ctx.clone(), $f).map_err(jerr)?;
                    g.set($name, f).map_err(jerr)?;
                }};
            }

            // ── 原语：签名尽量简单，legado 那套形状交给 PRELUDE ──
            prim!("__ajax", {
                let h = host.clone();
                move |url: String| -> String {
                    if url.trim().is_empty() {
                        String::new()
                    } else {
                        h.ajax(&url)
                    }
                }
            });
            prim!("__setContent", {
                let h = host.clone();
                move |html: String| h.set_content(&html)
            });
            prim!("__rule", {
                let h = host.clone();
                move |rule: String, mode: String| -> String {
                    let m = match mode.as_str() {
                        "l" => Mode::List,
                        "o" => Mode::Html,
                        _ => Mode::Text,
                    };
                    if rule.is_empty() {
                        String::new()
                    } else {
                        h.rule(&rule, m)
                    }
                }
            });
            prim!("__put", {
                let h = host.clone();
                let vars = vars.clone();
                move |k: String, v: String| -> String {
                    if !k.is_empty() {
                        h.put(&k, &v);
                        vars.borrow_mut().insert(k, v.clone());
                    }
                    v
                }
            });
            prim!("__get", {
                let vars = vars.clone();
                move |k: String| -> String { vars.borrow().get(&k).cloned().unwrap_or_default() }
            });
            prim!("__sel", {
                let h = host.clone();
                move |html: String, sel: String| -> String { h.html_select(&html, &sel) }
            });
            prim!("__text", {
                let h = host.clone();
                move |html: String| -> String { h.html_text(&html) }
            });
            prim!("__attr", {
                let h = host.clone();
                move |html: String, n: String| -> String { h.html_attr(&html, &n) }
            });
            prim!("__remove", {
                let h = host.clone();
                move |html: String, sel: String| -> String { h.html_remove(&html, &sel) }
            });

            prim!("__md5", |s: String| digest_hex("md5", &s));
            prim!("__sha1", |s: String| digest_hex("sha1", &s));
            prim!("__sha256", |s: String| digest_hex("sha256", &s));
            prim!("__b64enc", |s: String| b64_encode(s.as_bytes()));
            prim!("__b64dec", |s: String| crate::net::decode(
                &b64_decode_bytes(&s),
                None
            ));
            // 字节数组用 latin-1 字符串在边界上搬运：每个 char 的码点 = 一个字节。
            // 比造一个「JS 数组 ↔ Rust Vec<u8>」的桥简单得多，也不会丢字节。
            prim!("__b64bytes", |s: String| {
                let out: String = b64_decode_bytes(&s).iter().map(|b| *b as char).collect();
                out
            });
            prim!("__decbytes", |bytes: String, charset: String| -> String {
                let raw: Vec<u8> = bytes.chars().map(|c| c as u32 as u8).collect();
                match encoding_rs::Encoding::for_label(charset.as_bytes()) {
                    Some(enc) => enc.decode(&raw).0.into_owned(),
                    None => crate::net::decode(&raw, None),
                }
            });
            prim!("__encodeURI", |s: String| percent_encode(&s));
            prim!("__timeFormat", |ms: f64| time_format(ms as i64));
            prim!("__log", |_: String| ());

            // ── 绑定 ──
            g.set("page", b.page).map_err(jerr)?;
            macro_rules! bind {
                ($name:literal, $v:expr) => {
                    g.set($name, $v).map_err(jerr)?;
                };
            }
            bind!("key", b.key.as_str());
            bind!("baseUrl", b.base_url.as_str());
            bind!("src", b.src.as_str());
            bind!("result", b.result.as_str());
            bind!("__sourceName", b.source_name.as_str());
            bind!("__sourceComment", b.source_comment.as_str());

            let chapter = Object::new(ctx.clone()).map_err(jerr)?;
            chapter
                .set("title", b.chapter_title.as_str())
                .map_err(jerr)?;
            chapter.set("url", b.chapter_url.as_str()).map_err(jerr)?;
            chapter.set("index", b.chapter_index).map_err(jerr)?;
            chapter.set("isVip", false).map_err(jerr)?;
            chapter.set("isPay", false).map_err(jerr)?;
            chapter.set("tag", "").map_err(jerr)?;
            g.set("chapter", chapter).map_err(jerr)?;

            let src = self.source_obj(&ctx, b)?;
            g.set("source", src).map_err(jerr)?;
            let book = Object::new(ctx.clone()).map_err(jerr)?;
            book.set("name", b.book_name.as_str()).map_err(jerr)?;
            book.set("author", b.book_author.as_str()).map_err(jerr)?;
            book.set("bookUrl", "").map_err(jerr)?;
            g.set("book", book).map_err(jerr)?;

            // ── prelude：legado 那套 API 的形状 ──
            eval_in(&ctx, PRELUDE).map_err(|why| Error::Js(format!("prelude 装配失败：{why}")))?;
            Ok(())
        })
    }

    /// `source` 对象。`getLoginHeaderMap()` 要返回**空对象**而不是 undefined ——
    /// 真书源里是 `source.getLoginHeaderMap().get("cookie")`，undefined 会直接抛。
    fn source_obj<'js>(&self, ctx: &rquickjs::Ctx<'js>, b: &Bindings) -> R<Object<'js>> {
        let jerr = |e: rquickjs::Error| Error::Js(e.to_string());
        let o = Object::new(ctx.clone()).map_err(jerr)?;
        o.set("bookSourceUrl", b.source_url.as_str())
            .map_err(jerr)?;
        o.set("bookSourceName", b.source_name.as_str())
            .map_err(jerr)?;
        o.set("bookSourceComment", b.source_comment.as_str())
            .map_err(jerr)?;
        o.set("bookSourceGroup", "").map_err(jerr)?;
        o.set("enabledCookieJar", false).map_err(jerr)?;
        let url = b.source_url.clone();
        let f = Function::new(ctx.clone(), move || url.clone()).map_err(jerr)?;
        o.set("getKey", f).map_err(jerr)?;
        let url = b.source_url.clone();
        let f = Function::new(ctx.clone(), move || url.clone()).map_err(jerr)?;
        o.set("getTag", f).map_err(jerr)?;
        Ok(o)
    }
}

/// legado 那套 `java.*` 的形状。**改这里就能扩宿主 API，不用动 Rust。**
///
/// 下面所有 `__xxx` 都是 Rust 装进来的原语。
const PRELUDE: &str = r#"
(function () {
  "use strict";
  var NL = "\n";
  function split(s) { return (typeof s === "string" && s !== "") ? s.split(NL) : []; }
  function join(a) { return a.join(NL); }

  // ── JSoup Elements 的简化版 ──
  // 每个元素就用它自己的 outerHTML 字符串表示；需要 HTML 操作时回 Rust。
  function Elements(list) { this._l = list || []; }
  Elements.prototype = {
    // 真书源里 `size()` 是方法（289 次），`length` 是属性（734 次）。
    // 别把 `size` 同时做成属性和方法 —— 那在 strict mode 下是 "no setter for property"。
    size: function () { return this._l.length; },
    get: function (i) { return new Elements(this._l.slice(i, i + 1)); },
    first: function () { return new Elements(this._l.slice(0, 1)); },
    last: function () { return new Elements(this._l.slice(-1)); },
    eq: function (i) { return this.get(i); },
    select: function (sel) {
      var out = [];
      for (var i = 0; i < this._l.length; i++) out = out.concat(split(__sel(this._l[i], sel)));
      return new Elements(out);
    },
    find: function (sel) { return this.select(sel); },
    attr: function (n) { return this._l.length ? __attr(this._l[0], n) : ""; },
    text: function () { return join(this._l.map(function (h) { return __text(h); })); },
    eachText: function () { return this._l.map(function (h) { return __text(h); }); },
    html: function () { return join(this._l); },
    outerHtml: function () { return join(this._l); },
    ownText: function () { return join(this._l.map(function (h) { return __text(h); })); },
    textNodes: function () { return this.eachText(); },
    tagName: function () { var m = /^<\s*([a-zA-Z0-9]+)/.exec(this._l[0] || ""); return m ? m[1] : ""; },
    id: function () { return this.attr("id"); },
    className: function () { return this.attr("class"); },
    hasAttr: function (n) { return this._l.length ? __attr(this._l[0], n) !== "" : false; },
    val: function () { return this.attr("value"); },
    remove: function (sel) {
      if (sel === undefined) { this._l = []; return this; }
      this._l = split(__remove(join(this._l), sel));
      return this;
    },
    toArray: function () { return this._l.slice(); },
    isEmpty: function () { return this._l.length === 0; },
    toString: function () { return join(this._l); },
    // `if (!list)` 在真书源里出现过，空集合要 falsy 有点反直觉，所以给个显式方法
    isNotEmpty: function () { return this._l.length > 0; }
  };
  Object.defineProperty(Elements.prototype, "length", {
    get: function () { return this._l.length; }
  });
  Elements.prototype.constructor = Elements;

  // ── java.* ──
  var java = {
    ajax: function (url) {
      if (url === null || url === undefined) return null;
      var u = (typeof url === "string") ? url : String(url[0]);
      var s = __ajax(u);
      return s === "" ? null : s;
    },
    getString: function (r) { return __rule(String(r), "s"); },
    getStringList: function (r) { return split(__rule(String(r), "l")); },
    getElement: function (r) { return new Elements(split(__rule(String(r), "o"))); },
    getElements: function (r) { return new Elements(split(__rule(String(r), "o"))); },
    setContent: function (h) { __setContent(String(h)); return ""; },
    put: function (k, v) { return __put(String(k), String(v)); },
    get: function (k) { return __get(String(k)); },
    log: function (s) { __log(String(s)); return ""; },
    toast: function (s) { __log(String(s)); return ""; },
    longToast: function (s) { __log(String(s)); return ""; },
    md5Encode: function (s) { return __md5(s === undefined || s === null ? "" : String(s)); },
    md5Encode16: function (s) { return __md5(s === undefined || s === null ? "" : String(s)).substring(8, 24); },
    sha1Encode: function (s) { return __sha1(String(s)); },
    sha256Encode: function (s) { return __sha256(String(s)); },
    base64Encode: function (s) { return __b64enc(String(s)); },
    base64Decode: function (s) { return __b64dec(String(s)); },
    base64DecodeToByteArray: function (s) {
      var t = __b64bytes(String(s)), a = [];
      for (var i = 0; i < t.length; i++) a.push(t.charCodeAt(i));
      return a;
    },
    encodeURI: function (s) { return __encodeURI(String(s)); },
    timeFormat: function (t) { return __timeFormat(Number(t)); },

    lang: {
      String: function (v, charset) {
        if (v !== null && v !== undefined && typeof v.length === "number" && typeof v !== "string") {
          var bytes = "";
          for (var i = 0; i < v.length; i++) bytes += String.fromCharCode(v[i] & 0xff);
          return __decbytes(bytes, charset === undefined ? "utf-8" : String(charset));
        }
        return String(v);
      },
      Integer: function (v) { return parseInt(v, 10) || 0; },
      Long: function (v) { return parseInt(v, 10) || 0; },
      Double: function (v) { return parseFloat(v) || 0; },
      Float: function (v) { return parseFloat(v) || 0; },
      Boolean: function (v) { return String(v) === "true" || v === 1; },
      Object: function () { return {}; },
      StringBuilder: function () {
        var s = "";
        return {
          append: function (x) { s += x; return this; },
          toString: function () { return s; }
        };
      }
    },

    // `java.security.MessageDigest.getInstance("MD5")` 那条路。
    // 真书源里就这三种算法，其余退回 md5 的形状。
    security: {
      MessageDigest: {
        getInstance: function (alg) {
          var a = String(alg).toUpperCase().replace(/[-_]/g, "");
          return {
            _buf: "",
            update: function (x) { this._buf += String(x); },
            reset: function () { this._buf = ""; },
            digest: function () {
              var hex = a.indexOf("SHA1") >= 0 ? __sha1(this._buf)
                : a.indexOf("SHA256") >= 0 ? __sha256(this._buf)
                : __md5(this._buf);
              var out = [];
              for (var i = 0; i < hex.length; i += 2) out.push(parseInt(hex.substr(i, 2), 16));
              return out;
            }
          };
        }
      }
    },
    util: { Base64: {
      getEncoder: function () { return { encodeToString: function (v) {
        var t = ""; for (var i = 0; i < v.length; i++) t += String.fromCharCode(v[i] & 0xff);
        return __b64enc(t);
      } }; },
      getDecoder: function () { return { decode: function (s) {
        var t = __b64bytes(String(s)), a = [];
        for (var i = 0; i < t.length; i++) a.push(t.charCodeAt(i));
        return a;
      } }; }
    } }
  };

  // 没实现的 Java 类（`javax.crypto.Cipher`、`java.sql.*` …）要**明确报出自己是谁**。
  //
  // 用 Proxy 而不是让它当 undefined：undefined 的报错是
  // 「cannot read property 'Cipher' of undefined」，完全指不出真正的原因；
  // 而这个项目里「某个书源用了我们没实现的 Java API」是最常见的失败，
  // 报清楚名字能省掉一轮排查。
  globalThis.java = new Proxy(java, {
    get: function (t, k) {
      if (typeof k !== "string" || k in t) return t[k];
      throw new Error("java." + k + " 未实现（本实现不做 JVM 类互操作）");
    }
  });

  // 没有 cookie jar。给个不崩的空壳（`cookie.removeCookie(...)` 很常见）。
  globalThis.cookie = {
    getCookie: function () { return ""; },
    getKey: function () { return ""; },
    setCookie: function () { return ""; },
    removeCookie: function () { return ""; },
    replaceCookie: function () { return ""; }
  };
  globalThis.cache = {
    get: function () { return ""; },
    put: function () { return ""; },
    delete: function () { return ""; }
  };

  // `java.setContent` 之后 source/book 上那几个字段还是有的
  globalThis.source.getLoginHeaderMap = function () { return { get: function () { return ""; } }; };
})();
"#;

/// 求值并**把异常里的原因挖出来**。
///
/// QuickJS 抛出的错误在 rquickjs 里只是 `Error::Exception`，`Display` 出来是
/// 「Exception generated by QuickJS」——不带任何原因。真正的信息在挂起的异常
/// 对象上，必须用 `Ctx::catch()` 取。少了这一步，所有 JS 报错都长一个样。
fn eval_in<'js>(ctx: &rquickjs::Ctx<'js>, src: &str) -> Result<rquickjs::Value<'js>, String> {
    use rquickjs::{CatchResultExt as _, CaughtError};
    match ctx.eval::<rquickjs::Value, _>(src.as_bytes()).catch(ctx) {
        Ok(v) => Ok(v),
        Err(CaughtError::Value(v)) => {
            let msg = v
                .as_exception()
                .and_then(|x| x.message())
                .unwrap_or_else(|| value_to_string(&v));
            Err(if msg.is_empty() {
                "JS 抛了个空异常".to_string()
            } else {
                msg
            })
        }
        Err(CaughtError::Exception(e)) => Err(e.message().unwrap_or_else(|| e.to_string())),
        Err(CaughtError::Error(e)) => Err(e.to_string()),
    }
}

// ── JS 值的边界处理 ──────────────────────────────────────────────────────

fn value_to_string(v: &rquickjs::Value<'_>) -> String {
    if v.is_null() || v.is_undefined() {
        return String::new();
    }
    if let Some(s) = v.as_string() {
        return s.to_string().unwrap_or_default();
    }
    if let Some(b) = v.as_bool() {
        return b.to_string();
    }
    if let Some(n) = v.as_int() {
        return n.to_string();
    }
    if let Some(f) = v.as_float() {
        return if f.fract() == 0.0 && f.abs() < 1e15 {
            format!("{f:.0}")
        } else {
            f.to_string()
        };
    }
    if let Some(a) = v.as_array() {
        let parts: Vec<String> = a
            .iter::<rquickjs::Value>()
            .filter_map(|v| v.ok())
            .map(|v| value_to_string(&v))
            .collect();
        return parts.join(",");
    }
    String::new()
}

fn to_out(v: &rquickjs::Value<'_>) -> Out {
    if v.is_null() || v.is_undefined() {
        return Out::Nothing;
    }
    if let Some(s) = v.as_string() {
        return Out::Str(s.to_string().unwrap_or_default());
    }
    if let Some(b) = v.as_bool() {
        return Out::Bool(b);
    }
    if let Some(n) = v.as_int() {
        return Out::Num(n as f64);
    }
    if let Some(f) = v.as_float() {
        return Out::Num(f);
    }
    if let Some(a) = v.as_array() {
        return Out::List(
            a.iter::<rquickjs::Value>()
                .filter_map(|v| v.ok())
                .map(|v| value_to_string(&v))
                .collect(),
        );
    }
    Out::Str(value_to_string(v))
}

// ── 编解码 ──────────────────────────────────────────────────────────────

fn digest_hex(alg: &str, data: &str) -> String {
    use md5::Digest as _;
    match alg {
        "sha1" => hex(&sha1::Sha1::digest(data.as_bytes())),
        "sha256" => hex(&sha2::Sha256::digest(data.as_bytes())),
        _ => hex(&md5::Md5::digest(data.as_bytes())),
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

pub fn b64_encode(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

pub fn b64_decode_bytes(s: &str) -> Vec<u8> {
    use base64::Engine as _;
    let cleaned: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    base64::engine::general_purpose::STANDARD
        .decode(cleaned.as_bytes())
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(cleaned.as_bytes()))
        .unwrap_or_default()
}

/// legado 的 `java.encodeURI` = `URLEncoder.encode(s, "UTF-8")`。
/// **注意它把空格编成 `+`**，和 JS 原生的 `encodeURIComponent` 不一样。
fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'*' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// legado 的 `java.timeFormat` 固定用 `yyyy/MM/dd HH:mm`（**本地时间**）。
fn time_format(ms: i64) -> String {
    use chrono::{Local, TimeZone as _};
    match Local.timestamp_millis_opt(ms).single() {
        Some(t) => t.format("%Y/%m/%d %H:%M").to_string(),
        None => String::new(),
    }
}

fn clip(s: &str) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= 60 {
        flat
    } else {
        flat.chars().take(60).collect::<String>() + "…"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一个只在内存里做假的宿主：字符串变换，方便断言「JS 到底把什么传下来了」。
    #[derive(Default)]
    struct Fake {
        vars: RefCell<HashMap<String, String>>,
        content: RefCell<String>,
    }

    impl Host for Fake {
        fn ajax(&self, url: &str) -> String {
            format!("<html>body of {url}</html>")
        }
        fn set_content(&self, html: &str) {
            *self.content.borrow_mut() = html.to_string();
        }
        fn rule(&self, rule: &str, mode: Mode) -> String {
            // 规则名以 empty 开头就当作「什么都没选到」，用来测空结果
            if rule.starts_with("empty") {
                return String::new();
            }
            let cur = self.content.borrow().clone();
            match mode {
                Mode::Text => format!("[{rule} on {cur}]"),
                Mode::List => format!("{rule}#1\n{rule}#2"),
                Mode::Html => format!("<p>{rule}</p>\n<p>{rule}2</p>"),
            }
        }
        fn put(&self, k: &str, v: &str) {
            self.vars.borrow_mut().insert(k.into(), v.into());
        }
        fn get(&self, k: &str) -> String {
            self.vars.borrow().get(k).cloned().unwrap_or_default()
        }
        fn html_select(&self, html: &str, sel: &str) -> String {
            format!("{html}+{sel}")
        }
        fn html_text(&self, html: &str) -> String {
            html.replace("<p>", "").replace("</p>", "")
        }
        fn html_attr(&self, html: &str, n: &str) -> String {
            if n == "href" {
                format!("http://a/{html}")
            } else {
                String::new()
            }
        }
        fn html_remove(&self, html: &str, sel: &str) -> String {
            html.replace(sel, "")
        }
    }

    fn js() -> Js {
        Js::new(
            Rc::new(Fake::default()),
            &Bindings {
                page: 3,
                key: "斗破".into(),
                base_url: "http://a.com/x/".into(),
                source_url: "http://a.com".into(),
                source_name: "测试源".into(),
                book_name: "斗破苍穹".into(),
                book_author: "天蚕土豆".into(),
                chapter_title: "第一章".into(),
                chapter_index: 1,
                ..Default::default()
            },
        )
        .unwrap()
    }

    fn t(src: &str) -> String {
        js().eval(src, None).unwrap().to_text()
    }

    #[test]
    fn plain_javascript_works() {
        assert_eq!(t("1+1"), "2");
        assert_eq!(t("'a'+'b'"), "ab");
        assert_eq!(t("[1,2,3].map(x=>x*2).join('-')"), "2-4-6");
        assert_eq!(t("JSON.stringify({a:1})"), r#"{"a":1}"#);
        assert_eq!(t(r#"JSON.parse('{"x":[1,2]}').x.length"#), "2");
        assert_eq!(
            t("(function(){let s=0;for(let i=0;i<5;i++)s+=i;return s})()"),
            "10"
        );
        assert_eq!(t("try{null.x}catch(e){'caught'}"), "caught");
        assert_eq!(t(r#"'a1 a2'.replace(/a(\d)/g,'[$1]')"#), "[1] [2]");
        assert_eq!(t("Math.round(1.5)"), "2");
        assert_eq!(t("'中文abc'.length"), "5");
        assert_eq!(t("String.fromCharCode(72,105)"), "Hi");
        assert_eq!(t("encodeURIComponent('斗破')"), "%E6%96%97%E7%A0%B4");
        assert_eq!(t("undefined"), "");
        assert_eq!(t("null"), "");
        assert_eq!(t("[1,2,3]").replace('\n', "|"), "1|2|3");
    }

    #[test]
    fn bindings_are_visible() {
        assert_eq!(t("page"), "3");
        assert_eq!(t("key"), "斗破");
        assert_eq!(t("baseUrl"), "http://a.com/x/");
        assert_eq!(t("source.bookSourceUrl"), "http://a.com");
        assert_eq!(t("source.getKey()"), "http://a.com");
        assert_eq!(t("book.name"), "斗破苍穹");
        assert_eq!(t("book.author"), "天蚕土豆");
        // 真书源里 `@js:` 常写 `chapter.title`
        assert_eq!(t("chapter.title"), "第一章");
        assert_eq!(t("chapter.index"), "1");
        assert_eq!(t("(page-1)*50"), "100");
        // 真书源里是 `source.getLoginHeaderMap().get("cookie")`，
        // 返回 undefined 会当场抛，所以必须是对象
        assert_eq!(t(r#"source.getLoginHeaderMap().get("cookie")"#), "");
        assert_eq!(t(r#"cookie.removeCookie("x")"#), "");
        assert_eq!(t(r#"cache.put("k","v")"#), "");
    }

    #[test]
    fn result_binding_is_per_eval() {
        let js = js();
        assert_eq!(
            js.eval("result", Some("上一轮")).unwrap().to_text(),
            "上一轮"
        );
        // 下一次没有 result 时不能残留
        assert_eq!(
            js.eval("typeof result", None).unwrap().to_text(),
            "undefined"
        );
    }

    #[test]
    fn host_primitives_are_reachable_through_the_legado_shape() {
        assert_eq!(
            t("java.ajax('http://x/1')"),
            "<html>body of http://x/1</html>"
        );
        // 失败给 null（真书源会判 falsy）
        assert_eq!(t("java.ajax('')"), "");
        assert_eq!(t("java.getString('class.a@text')"), "[class.a@text on ]");
        assert_eq!(
            t("java.setContent('<p>hi</p>'); java.getString('r')"),
            "[r on <p>hi</p>]"
        );
        assert_eq!(t("java.getStringList('r').join('|')"), "r#1|r#2");
        assert_eq!(t("java.getStringList('r').length"), "2");
        assert_eq!(t("java.put('k','v'); java.get('k')"), "v");
        assert_eq!(t("java.get('没存过')"), "");
        assert_eq!(t("java.log('x')"), "");
    }

    #[test]
    fn elements_wrapper_matches_jsoup_shape() {
        // 真书源里元素对象用的是：length / size() / get(i) / select() / attr() /
        // text() / html() / first() / toArray() / toString()
        assert_eq!(t("java.getElements('r').length"), "2");
        assert_eq!(t("java.getElements('r').size()"), "2");
        assert_eq!(t("java.getElement('r').length"), "2");
        assert_eq!(t("java.getElements('r').get(0).toString()"), "<p>r</p>");
        assert_eq!(t("java.getElements('r').first().toString()"), "<p>r</p>");
        assert_eq!(t("java.getElements('r').toArray().length"), "2");
        assert_eq!(t("java.getElements('r').text()"), "r\nr2");
        assert_eq!(t("java.getElements('r').select('.x').length"), "2");
        assert_eq!(t("java.getElements('r').attr('href')"), "http://a/<p>r</p>");
        // 没实现的属性给空串，不抛
        assert_eq!(t("java.getElements('r').attr('nonexistent')"), "");
        assert_eq!(t("java.getElements('r').tagName()"), "p");
        assert_eq!(t("java.getElement('empty').length"), "0");
        // 空集合上继续链式调用不能炸
        assert_eq!(t("java.getElements('r').remove().length"), "0");
        assert_eq!(t("java.getElement('empty').select('x').length"), "0");
    }

    #[test]
    fn digest_and_encoding_helpers_use_standard_vectors() {
        assert_eq!(
            t("java.md5Encode('abc')"),
            "900150983cd24fb0d6963f7d28e17f72"
        );
        assert_eq!(t("java.md5Encode16('abc')"), "3cd24fb0d6963f7d");
        assert_eq!(
            t("java.sha1Encode('abc')"),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(
            t("java.sha256Encode('abc')"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(t("java.base64Encode('abc')"), "YWJj");
        assert_eq!(t("java.base64Decode('YWJj')"), "abc");
        assert_eq!(t("java.base64DecodeToByteArray('YWJj').length"), "3");
        assert_eq!(t("java.base64DecodeToByteArray('YWJj')[0]"), "97");
        // 中文往返
        assert_eq!(
            t("java.base64Decode(java.base64Encode('斗破苍穹'))"),
            "斗破苍穹"
        );
        // 坏输入不能抛
        assert_eq!(t("java.base64Decode('!!!not base64!!!')"), "");
        assert_eq!(
            t("java.md5Encode(undefined)"),
            "d41d8cd98f00b204e9800998ecf8427e"
        );
        // java.encodeURI 是 URLEncoder 语义（空格变 +），和 encodeURIComponent 不同
        assert_eq!(t("java.encodeURI('a b')"), "a+b");
        assert_eq!(
            t("java.encodeURI('a b') === encodeURIComponent('a b')"),
            "false"
        );
    }

    #[test]
    fn java_interop_for_the_common_cases() {
        assert_eq!(t("java.lang.String('x')"), "x");
        assert_eq!(t("java.lang.Integer('42')+1"), "43");
        assert_eq!(t("java.lang.Boolean('true')"), "true");
        // 字节数组 → 字符串（这是 `new java.lang.String(bytes)` 的常见写法）
        assert_eq!(
            t("java.lang.String(java.base64DecodeToByteArray('5paX56C0'))"),
            "斗破"
        );
        assert_eq!(
            t(r#"java.lang.String(java.base64DecodeToByteArray('5paX56C0'), "utf-8")"#),
            "斗破"
        );
        // MessageDigest 那条路
        assert_eq!(
            t(
                r#"var d=java.security.MessageDigest.getInstance("MD5"); d.update("abc"); var h=""; var b=d.digest(); for(var i=0;i<b.length;i++){h+=("0"+(b[i]&255).toString(16)).slice(-2)} h"#
            ),
            "900150983cd24fb0d6963f7d28e17f72"
        );
        // java.util.Base64
        assert_eq!(
            t("java.util.Base64.getEncoder().encodeToString([97,98,99])"),
            "YWJj"
        );
        // 没实现的工具类不能崩，只是 undefined
        // 没实现的 Java 类要报出它自己的名字，而不是「cannot read property of undefined」
        let e = js().eval("java.sql.Connection", None).unwrap_err();
        let msg = format!("{e}");
        assert!(msg.contains("java.sql"), "{msg}");
        assert!(msg.contains("未实现"), "{msg}");
        // 已实现的不能被 Proxy 挡住
        assert_eq!(t("typeof java.lang.String"), "function");
        assert_eq!(t("typeof java.util.Base64"), "object");
    }

    #[test]
    fn time_format_is_legado_format_local_time() {
        let s = t("java.timeFormat(1700000000000)");
        assert_eq!(s.len(), 16, "{s}");
        assert!(s.starts_with("2023/"), "{s}");
        assert!(s.contains('/') && s.contains(':'), "{s}");
    }

    #[test]
    fn script_errors_are_reported_not_swallowed() {
        let e = js().eval("this is not javascript ((", None);
        assert!(matches!(e, Err(Error::Js(_))), "{e:?}");
        // 报错信息里要带上脚本片段，否则根本不知道是哪条规则坏了
        let msg = format!("{}", e.unwrap_err());
        assert!(msg.contains("not javascript"), "{msg}");
    }

    /// 在「和真跑时一样大的线程栈」里跑 —— `online.rs` 的 worker 就是这么起的。
    /// 默认的测试线程栈太小，JS 栈上限和它就分不开了。
    fn on_worker_stack<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        std::thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn(f)
            .expect("起线程")
            .join()
            .expect("线程里不该 panic")
    }

    #[test]
    fn hostile_scripts_do_not_take_down_the_process() {
        on_worker_stack(|| {
            // 递归爆栈要变成**可捕获的 JS 错误**，不能把进程带崩
            let r = js().eval("(function f(){return f()})()", None);
            assert!(r.is_err(), "无限递归应当报错，实际 {r:?}");
            // 语法垃圾：报错就行
            for bad in ["{", ")", "function", "'未闭合", "`${", "/*", "var 1x"] {
                let _ = js().eval(bad, None);
            }
            // 深度嵌套的合法代码
            let deep = format!("{}1{}", "(".repeat(500), ")".repeat(500));
            let _ = js().eval(&deep, None);
            // 死循环不能真的跑不完：宿主层要能中断。
            // QuickJS 没有步数上限，所以真跑时靠 worker 线程超时（见 online.rs）。
            // 这里只验证「长但有限的循环能跑完」。
            let r = js().eval("let x=0; for (let i=0;i<100000;i++) x+=i; x", None);
            assert_eq!(r.unwrap().to_text(), "4999950000");
        });
    }

    /// 把真书源集合里**全部** JS 片段喂一遍。
    ///
    /// 这条测试的价值不在断言，而在**跑得完**：真 JS 是 QuickJS 直接执行的，
    /// 语法垃圾、没实现的 Java API、深递归都会在这里现形。
    /// 它同时给出「这些脚本里有多少能真的跑通」这个数字 —— 见下面的 `report`。
    #[test]
    fn every_real_world_js_snippet_runs_or_fails_cleanly() {
        let corpus = include_str!("../tests/fixtures/rule-corpus.txt");
        let js = js();
        // 分隔符用的是回车符：真规则里含换行，用换行符切会把 JS 脚本切碎
        let (mut ok, mut err) = (0usize, 0usize);
        let mut first_errors: Vec<String> = Vec::new();
        for line in corpus.split('\r') {
            for snippet in snippets_of(line) {
                match js.eval(&snippet, Some("样例正文 result 123")) {
                    Ok(_) => ok += 1,
                    Err(e) => {
                        err += 1;
                        // 只留前几条，输出别把日志淹掉
                        if first_errors.len() < 12 {
                            first_errors.push(format!("{} ← {e}", clip(&snippet)));
                        }
                    }
                }
            }
        }
        let total = ok + err;
        assert!(total > 1500, "语料里只捞出 {total} 条 JS，应该更多");
        println!(
            "JS 片段 {total} 条：能跑 {ok} 条（{}%），报错 {err} 条",
            ok * 100 / total
        );
        // 按原因归类 —— 单看「报错 700 条」没用，要知道缺的是哪一类 API
        let mut kinds: std::collections::BTreeMap<String, usize> = Default::default();
        for e in &first_errors {
            if let Some(at) = e.find("← JS 出错：") {
                let why = &e[at + "← JS 出错：".len()..];
                let key: String = why.chars().take(46).collect();
                *kinds.entry(key).or_default() += 1;
            }
        }
        for (k, n) in &kinds {
            println!("  {n:4} × {k}");
        }
        for e in first_errors.iter().take(6) {
            println!("  ✗ {e}");
        }
    }

    /// 从一条规则串里把 JS 片段捞出来：`@js:` 吃到底、`<js>…</js>`、以及
    /// `{{ }}` 里不是纯算术的表达式。
    fn snippets_of(line: &str) -> Vec<String> {
        let mut out = Vec::new();
        let lower = line.to_ascii_lowercase();
        if let Some(i) = lower.find("@js:") {
            out.push(line[i + 4..].to_string());
        }
        if let Some(i) = lower.find("<js>") {
            let rest = &line[i + 4..];
            let end = lower[i + 4..].find("</js>").unwrap_or(rest.len());
            out.push(rest[..end].to_string());
        }
        let bytes = line.as_bytes();
        let mut i = 0;
        while let Some(at) = line[i..].find("{{") {
            let start = i + at;
            let Some(rel) = line[start + 2..].find("}}") else {
                break;
            };
            let expr = line[start + 2..start + 2 + rel].trim();
            // 三类都不该当 JS 跑：
            // * 纯算术（内置求值器已覆盖）；
            // * `{{@@class.x@text}}` / `{{$.x}}` —— legado 的 `isRule()` 判定它们是**规则**；
            // * 空的。
            if !expr.is_empty()
                && !crate::rule::is_rule_pub(expr)
                && crate::rule::eval_expr_pub(expr, "k", 1).is_none()
            {
                out.push(expr.to_string());
            }
            i = start + 2 + rel + 2;
            if i >= bytes.len() {
                break;
            }
        }
        out
    }

    #[test]
    fn vars_round_trip_to_the_caller() {
        // `@put:` / `@get:` 的变量要能被外面取回，否则下次求值就丢了
        let js = js();
        js.eval(r#"java.put("token", "abc123")"#, None).unwrap();
        assert_eq!(
            js.vars.borrow().get("token").map(String::as_str),
            Some("abc123")
        );
    }
}
