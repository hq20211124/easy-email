"""Report what a real user.db looks like, for before/after a migration check."""

import os
import sqlite3
import sys

path = os.path.expandvars(r"%APPDATA%\com.easyemail.desktop\user.db")
label = sys.argv[1] if len(sys.argv) > 1 else "now"

con = sqlite3.connect(path)
version = con.execute("pragma user_version").fetchone()[0]
tables = sorted(
    row[0]
    for row in con.execute("select name from sqlite_master where type = 'table'")
)
counts = {}
for table in ("drafts", "outbox", "pending_ops"):
    try:
        counts[table] = con.execute(f"select count(*) from {table}").fetchone()[0]
    except sqlite3.Error as error:  # a table that does not exist yet is the point of the check
        counts[table] = f"<{error}>"

print(f"{label}: user_version = {version}")
print(f"{label}: tables = {tables}")
print(f"{label}: rows = {counts}")
