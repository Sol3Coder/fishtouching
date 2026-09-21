"""真书源里 XPath 的实际用法。

和 JSON 路径一样：先量再写。XPath 1.0 是庞大的规范，但真书源用到的
很可能是个小集合。
"""
import io
import json
import re
import sys
from collections import Counter

SRC = "fixtures/legado-sources-3907.json"  # 不入库，见 .gitignore
RULE_FIELDS = ["ruleSearch", "ruleBookInfo", "ruleToc", "ruleContent",
               "ruleExplore", "ruleReview"]

data = json.load(io.open(SRC, encoding="utf-8"))


def split_rule(s, seps=("&&", "||", "%%")):
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


def xpath_parts(rule):
    """从一条规则里抽出 XPath 表达式。"""
    r = rule.strip()
    if not r:
        return []
    # `@js:` 之后是脚本，别在里面找选择器
    r = r.split("@js:")[0]
    if r.lower().startswith("<js>"):
        return []
    out = []
    for part in split_rule(r):
        p = part.strip()
        if not p:
            continue
        if p.startswith("@@"):
            p = p[2:].strip()
        if p.lower().startswith("@xpath:"):
            p = p[7:].strip()
        if p.startswith("//") or (p.startswith("/") and len(p) > 1) or p.startswith("("):
            out.append(p)
    return out


xp = []
for s in data:
    if not isinstance(s, dict):
        continue
    for rf in RULE_FIELDS:
        obj = s.get(rf)
        if isinstance(obj, dict):
            for v in obj.values():
                if isinstance(v, str):
                    xp.extend(xpath_parts(v))

print(f"XPath 表达式 {len(xp)} 条，去重 {len(set(xp))}")

FEATURES = [
    ("// 后代",           lambda e: "//" in e),
    ("/ 子级",            lambda e: "/" in e),
    ("* 通配",            lambda e: re.search(r"(^|/)\*", e) is not None),
    ("[@a='v'] 属性等于",  lambda e: re.search(r"\[@[\w:-]+\s*=", e) is not None),
    ("[@a] 属性存在",      lambda e: re.search(r"\[@[\w:-]+\s*\]", e) is not None),
    ("[n] 位置",          lambda e: re.search(r"\[\d+\]", e) is not None),
    ("[last()]",          lambda e: "last()" in e),
    ("[position()=]",     lambda e: "position()" in e),
    ("text()",            lambda e: "text()" in e),
    ("@attr 结尾",         lambda e: re.search(r"/@[\w:-]+\s*$", e) is not None),
    (":: 轴",             lambda e: "::" in e),
    ("following-sibling", lambda e: "following-sibling" in e),
    ("preceding-sibling", lambda e: "preceding-sibling" in e),
    ("parent/ancestor",   lambda e: "ancestor" in e or "parent::" in e),
    ("contains(",         lambda e: "contains(" in e),
    ("starts-with(",      lambda e: "starts-with(" in e),
    ("normalize-space(",  lambda e: "normalize-space(" in e),
    ("and / or",          lambda e: re.search(r"\b(and|or)\b", e) is not None),
    ("| 并集",            lambda e: "|" in e),
    ("string( / concat(", lambda e: re.search(r"\b(string|concat|substring|translate)\(", e) is not None),
    ("[. = ] 文本谓词",    lambda e: re.search(r"\[\s*\.\s*=", e) is not None),
    ("() 分组",           lambda e: e.startswith("(")),
]

print("\n── 特征（按条数） ──")
for name, fn in FEATURES:
    n = sum(1 for e in xp if fn(e))
    if n:
        print(f"  {name:22} {n:5}  ({n*100//max(len(xp),1)}%)")

print("\n── 去重后最常见的 30 条 ──")
for e, c in Counter(xp).most_common(30):
    print(f"  {c:4}  {e[:105]}")

print("\n── 轴的分布 ──")
axes = Counter()
for e in xp:
    for m in re.finditer(r"([\w-]+)::", e):
        axes[m.group(1)] += 1
for a, c in axes.most_common():
    print(f"  {a:20} {c}")

print("\n── 谓词形状（去重） ──")
preds = Counter()
for e in xp:
    for m in re.finditer(r"\[([^\]]*)\]", e):
        s = m.group(1)
        s = re.sub(r"'[^']*'|\"[^\"]*\"", "Q", s)
        s = re.sub(r"[A-Za-z_][\w-]*", "K", s)
        s = re.sub(r"-?\d+", "N", s)
        preds[s] += 1
for p, c in preds.most_common(22):
    print(f"  {c:5}  [{p[:70]}]")
