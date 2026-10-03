"""Crash-recovery probe for a **real** profile (`design/07` §5.3.1).

What it does: inserts one `outbox` row that looks like a crash between "the worker claimed this
message" and "the bytes were written" — `state = 'sending'`, **no spool file**, **no frozen
Message-ID** — then you start the app and watch what recovery decides.

Why it is safe to run against a real account:

* Recovery's question is "did this already go out?". With no Message-ID it cannot search the Sent
  folder, so it answers `AskUser(NoMessageId)`: **no IMAP or SMTP connection is opened at all.**
* The row's payload is addressed to the account holder themselves, and the message is never sent.

What it proves (and the reason it exists): recovery runs **before** the first send pass, so a crashed
message is never re-sent by a worker that merely saw a due row.

Usage:
    python tools/probe-crash-recovery.py insert
    <start the app, wait for startup>
    python tools/probe-crash-recovery.py report
    python tools/probe-crash-recovery.py clean
"""

import os
import sqlite3
import sys
import time

DB = os.path.join(os.environ["APPDATA"], "com.easyemail.desktop", "user.db")
SUBJECT = "崩溃恢复探针"


def connect():
    return sqlite3.connect(DB)


def insert():
    conn = connect()
    account = conn.execute("select id from accounts").fetchone() if has_accounts(conn) else None
    account_id = account[0] if account else "probe@example.invalid"
    payload = (
        '{"to":["me@example.com"],"cc":[],"bcc":[],"subject":"%s",'
        '"body_text":"这条永远不会被发出去。","body_html":null,"in_reply_to":null,'
        '"references":[],"attachments":[],"message_id":null}' % SUBJECT
    )
    cursor = conn.execute(
        "insert into outbox (account_id, draft_id, payload_json, state, attempts, undo_until,"
        " scheduled_at, spool_path, spool_bytes, created_at, sent_message_id, last_error)"
        " values (?, null, ?, 'sending', 1, 0, 0, null, 0, ?, null, null)",
        (account_id, payload, int(time.time())),
    )
    conn.commit()
    print("inserted id", cursor.lastrowid, "for", account_id)
    conn.close()


def has_accounts(conn):
    try:
        conn.execute("select id from accounts limit 1")
        return True
    except sqlite3.OperationalError:
        return False


def report():
    conn = connect()
    rows = conn.execute(
        "select id, state, attempts, spool_path, sent_message_id, last_error from outbox"
    ).fetchall()
    for row in rows:
        print(row)
    if not rows:
        print("no outbox rows")
    conn.close()


def clean():
    conn = connect()
    removed = conn.execute("delete from outbox where state = 'needs_attention'").rowcount
    conn.commit()
    print("removed", removed, "needs_attention row(s); outbox now", end=" ")
    print(conn.execute("select count(*) from outbox").fetchone()[0], "row(s)")
    conn.close()


if __name__ == "__main__":
    action = sys.argv[1] if len(sys.argv) > 1 else "report"
    {"insert": insert, "report": report, "clean": clean}[action]()
