"""静态覆盖率：3907 个真书源里，有多少「按现在的实现应该能跑」。

判据（一个源算「能跑」当且仅当全部满足）：
  * enabled 不是 false，bookSourceType 是 0（文本）
  * 有 searchUrl，且有 ruleSearch.bookList / ruleToc.chapterList / ruleContent.content
  * 所有规则串里**没有**需要我们不支持的能力的东西：
      - XPath（`//`、`/` 开头、`@xpath:`）
      - `@webjs:`（要真浏览器）
      - AES 类 Java 调用
      - `@get:` / `@put:` 之外的未知 `@` 选项（暂不算阻塞）
  * JSON 路径算不算阻塞 —— 由命令行开关决定，用来给出改动前后的对比

用法：python survey_coverage.py [--json-blocked]
"""
import io
import json
import re
import sys

SRC = "fixtures/legado-sources-3907.json"  # 不入库，见 .gitignore
RULE_FIELDS = ["ruleSearch", "ruleBookInfo", "ruleToc", "ruleContent",
               "ruleExplore", "ruleReview"]

JSON_BLOCKED = "--json-blocked" in sys.argv
XPATH_BLOCKED = "--xpath-blocked" in sys.argv

data = json.load(io.open(SRC, encoding="utf-8"))


def split_rule(s, seps=("&&", "||", "%%")):
    """平衡组切分：跳过 [...] / (...) 内部和引号内部。"""
    out, cur = [], []
    depth, quote, i = 0, None, 0
    while i < len(s):
        c = s[i]
        if quote:
            cur.append(c)
            if c == quote and (i == 0 or s[i - 1] != "\\"):
                quote = None
            i += 1
            continue
        if c in "'\"":
            quote = c
            cur.append(c)
            i += 1
            continue
        if c in "[(":
            depth += 1
        elif c in "])":
            depth -= 1
        if depth <= 0:
            for sep in seps:
                if s.startswith(sep, i):
                    out.append("".join(cur))
                    cur = []
                    i += len(sep)
                    break
            else:
                cur.append(c)
                i += 1
            continue
        cur.append(c)
        i += 1
    out.append("".join(cur))
    return out


def needs(rule):
    """这条规则需要什么我们没实现的能力？返回一组标签。"""
    bad = set()
    r = rule.strip()
    if not r:
        return bad
    low = r.lower()
    if "@webjs:" in low:
        bad.add("webjs")
    if re.search(r"aesBase64|aesEncode|aesDecode|desBase64|rsaEncode", r):
        bad.add("aes")

    # 把 `@js:` 之后的部分摘掉再找选择器特征 —— 脚本里什么字符都可能有
    head = r.split("@js:")[0].lower()
    if head.startswith("<js>"):
        head = ""
    for part in split_rule(head):
        p = part.strip().lstrip("@ \t")
        p = p[2:].strip() if p.startswith("@@") else p
        if XPATH_BLOCKED:
            if p.lower().startswith("@xpath:"):
                bad.add("xpath")
            elif p.startswith("//"):
                bad.add("xpath")
            elif p.startswith("/") and not p.startswith("//") and len(p) > 1 and p[1] != "/":
                # 单个 `/` 开头：legado 认成 XPath。要排除 URL（`http://` 已处理）
                bad.add("xpath")

    if JSON_BLOCKED and (r.startswith("$.") or r.startswith("$[") or low.startswith("@json:")):
        bad.add("jsonpath")
    return bad


def rule_strings(s):
    out = []
    for rf in RULE_FIELDS:
        obj = s.get(rf)
        if isinstance(obj, dict):
            for v in obj.values():
                if isinstance(v, str) and v.strip():
                    out.append(v)
        elif isinstance(obj, str) and obj.strip():
            out.append(obj)
    return out


stats = {"total": 0, "no_search": 0, "incomplete": 0, "runnable": 0}
blockers = {}
domains = {}

for s in data:
    if not isinstance(s, dict):
        continue
    if s.get("enabled") is False:
        continue
    if s.get("bookSourceType", 0) not in (0, None):
        continue
    stats["total"] += 1

    if not s.get("searchUrl"):
        stats["no_search"] += 1
        continue

    rs = s.get("ruleSearch") or {}
    tc = s.get("ruleToc") or {}
    ct = s.get("ruleContent") or {}
    if not (isinstance(rs, dict) and rs.get("bookList")
            and isinstance(tc, dict) and tc.get("chapterList")
            and isinstance(ct, dict) and ct.get("content")):
        stats["incomplete"] += 1
        continue

    bad = set()
    for r in rule_strings(s):
        bad |= needs(r)

    if bad:
        for b in bad:
            blockers[b] = blockers.get(b, 0) + 1
        continue

    stats["runnable"] += 1
    host = (s.get("bookSourceUrl") or "").split("//")[-1].split("/")[0]
    domains[host] = domains.get(host, 0) + 1

mode = ("JSON 路径算阻塞（改动前）" if JSON_BLOCKED
        else "XPath 算阻塞（加 XPath 之前）" if XPATH_BLOCKED
        else "两者都支持（现在）")
print(f"══ {mode} ══")
print(f"  启用的文本源        {stats['total']}")
print(f"  不能搜 / 不完整      {stats['no_search']} + {stats['incomplete']}")
print(f"  **应该能跑**        {stats['runnable']}  ({stats['runnable']*100//max(stats['total'],1)}%)")
print(f"  去重后的域名        {len(domains)}")
print("  剩余阻塞（一个源可能中多条）：")
for k, v in sorted(blockers.items(), key=lambda x: -x[1]):
    print(f"    {k:10} {v}")
