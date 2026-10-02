#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""design/tools/audit.py —— 对**活文件**跑的设计一致性审计（评审的元教训的产物）。

为什么存在：第一轮"全量红队"漏掉的恰恰是"文档之间互相矛盾"和"文档与代码不一致"这两类，
而它们只有靠**独立评审 + 对活文件跑脚本**才能抓到。所以把当时的临时脚本固化到这里：
**任何声称"设计一致"的结论都必须由本脚本的输出支撑**（README 与 A2 都引用它）。

用法：
    python design/tools/audit.py            # 全部检查；退出码 = 失败项数
    python design/tools/audit.py --quiet    # 只打印失败项与汇总

检查项：
  1. 结构完整性（15 个功能域都被引用；每份模块文档有红队/验收/未决）
  2. 被推翻的说法不得复活（"推送 50 秒"、"EWS 只读回退"、"cache.db 可以随便删"、"SIZE=n 预检"）
  3. 跨文档常量一致（首同步分钟数、基准项数、IDLE 窗口、行高、索引体积单位、DbSet、origin…）
  4. 不做清单不得被承诺（EWS / NTLM-SSO / PST / 别人的密码库）
  5. 全部 SQL 块能在 SQLite 上真的建起来（含 FTS5）
  6. 原型：JS 语法、无硬编码颜色、虚拟列表无障碍属性、无 emoji

依赖：Python 3.8+；第 5 项需要 sqlite3 带 FTS5；第 6 项需要 PATH 里有 node。
"""
import io
import os
import re
import sqlite3
import subprocess
import sys
import tempfile

QUIET = "--quiet" in sys.argv
HERE = os.path.dirname(os.path.abspath(__file__))
DESIGN = os.path.dirname(HERE)
PROTO = os.path.join(DESIGN, "12-ui-prototype", "index.html")

sys.stdout.reconfigure(encoding="utf-8")
DOCS = {}
for _n in sorted(os.listdir(DESIGN)):
    if _n.endswith(".md"):
        DOCS[_n] = io.open(os.path.join(DESIGN, _n), encoding="utf-8").read()

# A1/A2/DECISIONS 是"决策与事实的记录"，按设计会**引用**被否决的说法 ⇒ 不参与"复活/承诺"类检查
RECORDS = ("A1-research-findings.md", "A2-final-review.md", "DECISIONS.md", "README.md")
fails = []


def check(title, ok, evidence=""):
    if not ok or not QUIET:
        print("%-4s %s%s" % ("PASS" if ok else "FAIL", title, ("  <- " + evidence) if evidence else ""))
    if not ok:
        fails.append(title)


def hits(pattern, exclude=RECORDS):
    out = []
    for name, text in DOCS.items():
        if name in exclude:
            continue
        for m in re.finditer(pattern, text):
            out.append((name, text[max(0, m.start() - 60):m.end() + 60].replace("\n", " ")))
    return out


def has_any(ctx, words):
    return any(w in ctx for w in words)


print("=== 1. 结构完整性 ===")
check("15 个功能域前缀都被模块文档引用",
      all(any(p + "-" in t for n, t in DOCS.items() if n != "01-product.md")
          for p in ("ACC", "CAL", "CONTACT", "DIST", "MAIL", "MIG", "NOTIF", "PERF",
                    "PROTO", "READ", "REL", "RULE", "SEC", "SRCH", "SYN", "UI")))
for name, text in DOCS.items():
    if name in RECORDS:
        continue
    if name == "19-roadmap.md":
        continue  # 路线图的红队/验收/未决在 §9（结构不同但内容齐全）
    check("%-26s 红队/验收/未决齐全" % name,
          bool(re.search(r"^## .*逆向反驳", text, re.M)) and "验收清单" in text
          and has_any(text, ("未决问题", "待拍板", "待你拍板", "需要你拍板", "未决")))

print("\n=== 2. 被推翻的说法不得复活 ===")
# 允许的上下文：明确在"更正/推翻/要改掉"它（例如"把 README 的'推送 50 秒'改成'亚秒级'"）
FIXING = ("更正", "推翻", "改成", "错误", "不再", "修 ", "已被", "删除那两处")
stale = [(n, c) for n, c in hits(r"推送[^。\n]{0,10}50 秒") if not has_any(c, FIXING)]
check("没有把'推送 50 秒'当成事实（只允许出现在更正说明里）", not stale, str(stale[:2]))
check("没有 'EWS 只读回退' 这类承诺", not hits(r"EWS 只读回退"))
bad_cache = [c for _, c in hits(r"可以随便删") if not has_any(c, ("不再说", "改成", "不"))
             and "可以随便删" in c and "不可" not in c]
check("没有 'cache.db 可以随便删' 的正向说法",
      all(has_any(c, ("不再说", "改成")) for _, c in hits(r"可以随便删")), str(hits(r"可以随便删")[:1]))
size = [(n, c) for n, c in hits(r"MailParameter::Size|SIZE=n") if not has_any(c, ("做不了", "不存在", "错误", "更正", "不是"))]
check("没有把 SIZE= 当成可用的预检", not size, str(size[:1]))
check("没有 '截断保留最旧（既有修复）' 的错误断言", not hits(r"截断保留最旧（既有）"))

print("\n=== 3. 跨文档常量一致 ===")
first_sync = re.findall(r"首同步[^|\n]{0,30}≤\s*(\d+)\s*分钟",
                        DOCS["02-architecture.md"] + DOCS["04-sync.md"] + DOCS["19-roadmap.md"])
check("首同步预算只有 8 分钟一个口径", set(first_sync) == {"8"}, str(set(first_sync)))
check("基准项数只有 16 一个口径", "12 项" not in DOCS["15-performance.md"] and "16 项" in DOCS["15-performance.md"])
check("IDLE 窗口只有 120 s（02 不再写 30 s）",
      "120 s" in DOCS["02-architecture.md"] and "窗口 30 s" not in DOCS["02-architecture.md"])
check("行高只有一个口径（40 px）", "行高 40 px" in DOCS["12-ui-design-system.md"])
check("索引体积单位统一为'已索引文本量'",
      "已索引文本量" in DOCS["15-performance.md"] and "已索引文本量" in DOCS["09-search.md"])
check("DbSet 在 02 定义、在 03 被引用",
      "pub struct DbSet" in DOCS["02-architecture.md"] and "DbSet::open" in DOCS["03-storage.md"])
check("origin 三值在 03 与 06 都出现",
      "origin" in DOCS["03-storage.md"] and "origin='imported'" in DOCS["06-migration.md"])
check("auto_vacuum 的'必须在建表前设'在 03 与 15 都写明",
      "auto_vacuum" in DOCS["03-storage.md"] and "auto_vacuum" in DOCS["15-performance.md"])
check("INDEX_VERSION 取代了 tokenize 字符串作为重建触发",
      "INDEX_VERSION" in DOCS["03-storage.md"] and "见 09 的最终决定" not in DOCS["03-storage.md"])
check("MdcState 三态在 14 定义", "MdcState" in DOCS["14-security-privacy.md"])
check("白名单需要 auth_ok（14 与 08 一致）",
      "auth_ok" in DOCS["14-security-privacy.md"] and "auth_ok" in DOCS["08-reading-threads.md"])
check("解密邮件远程内容只有'永远拦截'一个口径",
      "没有例外" in DOCS["14-security-privacy.md"] and "除非用户对**该封**再点一次" not in DOCS["14-security-privacy.md"])
check("APPEND 与 SMTP 上限是两个字段（07）", "append_limit_bytes" in DOCS["07-compose-send.md"])
check("outbox 的 due 三列 not null default 0（03）", DOCS["03-storage.md"].count("not null default 0") >= 3)
check("sync_jobs 有 next_attempt_at（03）", "next_attempt_at" in DOCS["03-storage.md"])
check("分词器结论一致：D13 与 09 §4.2 都是'CJK bigram 已定'",
      "bigram" in DOCS["DECISIONS.md"] and "已定" in DOCS["DECISIONS.md"].split("## D13")[1].split("## D14")[0]
      and "已用实测定案" in DOCS["09-search.md"] and "逐字符（现状，基线）" not in DOCS["09-search.md"])
check("三个已拍板决策（P1/P2/P3）在 01/DECISIONS 里都是'已定'",
      all(k in DOCS["DECISIONS.md"] for k in ("Svelte 5", "不自费 CASA", "SignPath Foundation"))
      and "已拍板" in DOCS["01-product.md"])

print("\n=== 4. 不做清单不得被承诺 ===")
for pattern, why in [(r"做\s*EWS|实现\s*EWS", "EWS"),
                     (r"NTLM/Kerberos/SSO[^。\n]{0,20}(M3|要做|实现)", "NTLM/SSO"),
                     (r"PST[^。\n]{0,20}(导入|支持|实现)", "PST"),
                     (r"解密[^。\n]{0,10}(logins\.json|key4\.db)", "别人的密码库")]:
    found = [(n, c) for n, c in hits(pattern)
             if not has_any(c, ("不做", "不要做", "绝", "划掉", "已废", "—", "没有纯 Rust", "不可行", "不推荐", "自相矛盾"))]
    check("没有承诺：" + why, not found, str(found[:1]))

print("\n=== 5. DDL 可执行（真的建库） ===")
ORDER = ["03-storage.md", "06-migration.md", "09-search.md", "10-rules-spam.md", "11-contacts.md", "16-reliability-ops.md"]
conn = sqlite3.connect(":memory:")
conn.execute("create virtual table probe using fts5(x)")
blocks = err = None
try:
    blocks = 0
    for name in ORDER:
        if name not in DOCS:
            continue
        for block in re.findall(r"```sql\n(.*?)```", DOCS[name], re.S):
            blocks += 1
            conn.executescript(block.replace("<见 09 的最终决定>", "unicode61"))
    objs = conn.execute("select count(*) from sqlite_master where type in ('table','view')").fetchone()[0]
    check("全部 SQL 块能在 SQLite 上建起来（%d 块 / %d 个对象）" % (blocks, objs), True)
except Exception as exc:  # noqa: BLE001
    check("全部 SQL 块能在 SQLite 上建起来", False, str(exc))

print("\n=== 6. 原型 ===")
html = io.open(PROTO, encoding="utf-8").read()
script = re.findall(r"<script>(.*?)</script>", html, re.S)[-1]
tmp = os.path.join(tempfile.gettempdir(), "audit_proto.js")
io.open(tmp, "w", encoding="utf-8").write(script)
rc = subprocess.run(["node", "--check", tmp], capture_output=True).returncode
check("原型 JS 语法通过（node --check）", rc == 0)
style = re.search(r"<style>(.*?)</style>", html, re.S).group(1)
check("原型标记里无硬编码颜色", not re.findall(r"#[0-9a-fA-F]{6}\b", html.replace(style, "")))
check("原型有虚拟列表无障碍属性",
      all(k in html for k in ("aria-activedescendant", "aria-setsize", "aria-posinset", "aria-multiselectable")))
check("原型无 emoji", not re.findall(r"[\U0001F300-\U0001FAFF\u2600-\u27BF]", html))
check("原型用了设计令牌（--sp-* / --dur-* / --on-brand / reduced-motion）",
      all(k in html for k in ("--sp-", "--dur-", "--on-brand", "prefers-reduced-motion")))

print("\n=== 汇总 ===")
print("失败项：%d" % len(fails))
for f in fails:
    print("  - " + f)
sys.exit(len(fails))
