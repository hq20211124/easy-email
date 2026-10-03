"""Report what a real local database looks like, for before/after a migration check.

    python tools/inspect-local-db.py <label> [user|cache]

Prints the SQLite `user_version`, the tables, and the row counts of the tables this project cares
about — which is what makes "the additive migration did not touch anything" checkable on a real
profile instead of being asserted from the shape of a `create table if not exists`.
"""

import os
import sqlite3
import sys

ROOT = os.path.expandvars(r"%APPDATA%\com.easyemail.desktop")
app = sys.argv[1] if len(sys.argv) > 1 else "now"
which = sys.argv[2] if len(sys.argv) > 2 else "user"

if which not in ("user", "cache"):
    sys.exit(f"unknown database {which!r}; expected 'user' or 'cache'")

path = os.path.join(ROOT, f"{which}.db")
con = sqlite3.connect(path)
version = con.execute("pragma user_version").fetchone()[0]
tables = sorted(
    row[0]
    for row in con.execute("select name from sqlite_master where type = 'table'")
)

counts = {}
for table in ("drafts", "outbox", "pending_ops", "quota_ledger", "messages", "sent_index"):
    try:
        counts[table] = con.execute(f"select count(*) from {table}").fetchone()[0]
    except sqlite3.Error as error:  # a table that does not exist yet is the point of the check
        counts[table] = f"<{error}>"

print(f"{which}.db {app}: user_version = {version}")
print(f"{which}.db {app}: tables = {tables}")
print(f"{which}.db {app}: rows = {counts}")
