"""What is actually in the two real databases — for "why can't I see / why isn't this grouped".

    python tools/inspect-cache-detail.py [substring-of-an-address]

Prints, read-only:
  * the stored sync window and every folder's cursors, with how much of each is cached (this is what
    answers "why does the list stop here": the list shows one page, the cache holds a window, and the
    server may simply have nothing older);
  * per-folder message counts and the oldest/newest cached date;
  * the messages matching one correspondent, with the fields that decide threading — `thread_key` from
    the cache, and `message_id` / `in_reply_to` / `references` from the stored body. A message with no
    `in_reply_to` and no `references` is **its own conversation**, however identical its subject is, so
    this is the first thing to look at when two rows "should" be one.

Written for a real report (2026-10-06) and kept, because both questions come back.
"""

import json
import os
import sqlite3
import sys

# Subjects are Chinese as often as not, and this console is not UTF-8. Without this the report comes
# back as mojibake, which is how a tool like this stops being read.
sys.stdout.reconfigure(encoding="utf-8", errors="replace")

ROOT = os.path.expandvars(r"%APPDATA%\com.easyemail.desktop")
who = f"%{sys.argv[1]}%" if len(sys.argv) > 1 else "%@%"
cache = sqlite3.connect(os.path.join(ROOT, "cache.db"))
user = sqlite3.connect(os.path.join(ROOT, "user.db"))
cache.row_factory = sqlite3.Row

print("=== sync window (user.db kv) ===")
for key, value in user.execute("select key, value from kv").fetchall():
    print(f"  {key} = {value}")

print("\n=== folders with cached mail ===")
rows = cache.execute(
    "select account_id, name, uidvalidity, last_uid, synced_at,"
    " (select count(*) from messages m where m.account_id = f.account_id and m.folder = f.name) as cached,"
    " (select min(uid) from messages m where m.account_id = f.account_id and m.folder = f.name) as min_uid,"
    " (select max(uid) from messages m where m.account_id = f.account_id and m.folder = f.name) as max_uid"
    " from folders f where cached > 0 order by account_id, name"
).fetchall()
for row in rows:
    print(
        f"  {row['account_id']:<22} {row['name']:<18} uidvalidity={row['uidvalidity']:<12}"
        f" last_uid={row['last_uid']:<7} cached={row['cached']:<5} uid {row['min_uid']}..{row['max_uid']}"
        f" synced_at={row['synced_at']}"
    )

print("\n=== date range of what is cached ===")
for row in cache.execute(
    "select account_id, folder, count(*) as n,"
    " datetime(min(date),'unixepoch','localtime') as oldest,"
    " datetime(max(date),'unixepoch','localtime') as newest"
    " from messages where account_id like '%qq.com' group by account_id, folder"
    " order by oldest limit 20"
).fetchall():
    print(f"  {row['account_id']:<22} {row['folder']:<18} n={row['n']:<5} {row['oldest']} .. {row['newest']}")

print(f"\n=== messages with {who} (thread_key decides the conversation) ===")
rows = cache.execute(
    "select m.folder, m.uid, m.date, m.subject, m.from_addr, m.to_addrs, m.thread_key,"
    " (select count(*) from messages x where x.account_id = m.account_id and x.thread_key = m.thread_key) as thread_size"
    " from messages m"
    " where (m.from_addr like ? or m.to_addrs like ?)"
    " order by m.date",
    (who, who),
).fetchall()
for row in rows:
    print(
        f"  {row['folder']:<14} uid={row['uid']:<7} {row['date']} thread_key={row['thread_key']!r}"
        f" size={row['thread_size']}"
    )
    print(f"      subject={row['subject']!r}")
    print(f"      from={row['from_addr']!r} to={row['to_addrs']!r}")

print("\n=== the stored bodies for those uids (the threading headers) ===")
for row in cache.execute(
    "select folder, uid, detail from bodies"
    " where uid in (select uid from messages where (from_addr like ? or to_addrs like ?))"
    " order by uid",
    (who, who),
).fetchall():
    detail = json.loads(row["detail"])
    print(
        f"  {row['folder']:<14} uid={row['uid']:<7} message_id={detail.get('message_id')!r}"
        f" in_reply_to={detail.get('in_reply_to')!r}"
    )
    print(f"      references={detail.get('references')!r}")
    print(f"      subject={detail.get('subject')!r}")
