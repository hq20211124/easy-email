"""Pick mail whose body is not on disk, then check whether the mode kept it off.

    python tools/probe-body-storage-report.py plan
    python tools/probe-body-storage-report.py verify-online target/probe-online.json
    python tools/probe-body-storage-report.py verify-cached target/probe-cached.json

The paired probe in `tools/probe-body-storage.mjs` opens mail in both modes and reads the stored
count back. On a real profile that count is a weak signal by itself: the background backfill is
writing bodies the whole time the probe runs, and a message whose body was already cached cannot
tell the two modes apart. So the candidates come from the database — only mail with no `bodies` row
can show a difference — and the verdict is read out of **the file's bytes**, which is what the mode
is actually about.

`cache.db-wal` is searched too: with WAL enabled a fresh write can live only in the log, and
"absent from the main file" would otherwise pass while the text sits one directory over.

The two arms use **disjoint** populations, and each arm is checked immediately after it runs. That
is not a nicety: the cached arm writes its messages to disk, so if both arms opened the same mail,
reading the file afterwards would find the online arm's snippets too and the online check would
prove nothing.
"""

import json
import os
import sqlite3
import sys

ROOT = os.path.expandvars(r"%APPDATA%\com.easyemail.desktop")
CACHE = os.path.join(ROOT, "cache.db")
PER_ARM = 4
POOL = 12
PLAN = os.path.join("target", "probe-plan.js")


def connection():
    # Opened read-only on purpose: the app owns this database while the probe runs.
    uri = "file:" + CACHE.replace("\\", "/") + "?mode=ro"
    con = sqlite3.connect(uri, uri=True)
    con.row_factory = sqlite3.Row
    return con


def uncached():
    """Every piece of mail on disk whose body is not — the only population the two arms can differ on."""
    con = connection()
    rows = con.execute(
        """
        select m.account_id, m.folder, m.uid, m.subject
          from messages m
          left join bodies b
            on b.account_id = m.account_id and b.folder = m.folder and b.uid = m.uid
         where b.uid is null
         order by m.folder, m.uid desc
        """
    ).fetchall()
    return [
        {
            "accountId": row["account_id"],
            "folder": row["folder"],
            "uid": row["uid"],
            "subject": row["subject"],
        }
        for row in rows
    ]


def spread(rows, count):
    """Take the newest few per folder, rotating, so one folder cannot carry the whole run."""
    by_folder = {}
    for item in rows:
        by_folder.setdefault(item["folder"], []).append(item)
    picked, round_index = [], 0
    while len(picked) < count:
        grew = False
        for folder in sorted(by_folder):
            if round_index < len(by_folder[folder]):
                picked.append(by_folder[folder][round_index])
                grew = True
                if len(picked) == count:
                    break
        if not grew:
            break
        round_index += 1
    return picked


def plan():
    """Hand the page a pool of mail that has no body on disk, newest first across folders."""
    rows = uncached()
    if len(rows) < POOL:
        sys.exit(f"only {len(rows)} messages without a body on disk; need {POOL}")
    body = {"pool": spread(rows, POOL), "need": PER_ARM, "restoreMode": "cached", "pool_total": len(rows)}
    write_plan(body)
    print(f"{len(rows)} uncached; offered {len(body['pool'])} to the page, wrote {PLAN}")
    for item in body["pool"]:
        print(f"  {item['folder']} uid={item['uid']}")


def pair(report_path):
    """Rebuild the plan around the mail the online arm actually managed to read.

    The second arm then opens **the same messages**, which is what makes the two byte checks a pair
    rather than two unrelated runs: each message is shown missing from the file after an 不落盘 read
    and present in it after a 缓存在本地 read.
    """
    with open(report_path, encoding="utf-8") as handle:
        report = json.load(handle)
    used = [
        {
            "accountId": item["accountId"],
            "folder": item["folder"],
            "uid": item["uid"],
            "snippet": item["snippet"],
        }
        for item in report["opened"]
        if item.get("snippet") and "error" not in item
    ]
    if len(used) < 2:
        sys.exit(f"the online arm produced {len(used)} usable messages; run it again")
    write_plan({"pool": used, "need": len(used), "restoreMode": "cached", "pool_total": len(used)})
    print(f"pairing the cached arm onto {len(used)} messages already proven absent:")
    for item in used:
        print(f"  {item['folder']} uid={item['uid']} snippet={item['snippet']!r}")


def write_plan(body):
    os.makedirs("target", exist_ok=True)
    with open(PLAN, "w", encoding="utf-8") as handle:
        handle.write("window.__bodyPlan = %s;\n" % json.dumps(body, ensure_ascii=False))


def file_bytes():
    blob = b""
    for name in ("cache.db", "cache.db-wal"):
        path = os.path.join(ROOT, name)
        if os.path.exists(path):
            with open(path, "rb") as handle:
                blob += handle.read()
    return blob


def verify(report_path, expect_present):
    with open(report_path, encoding="utf-8") as handle:
        report = json.load(handle)
    blob = file_bytes()
    con = connection()
    results, skipped = [], []
    for item in report.get("opened", []):
        if "error" in item:
            # Mail the server would not hand over cannot be evidence in either direction.
            skipped.append({"folder": item["folder"], "uid": item["uid"], "error": item["error"]})
            continue
        if not item.get("snippet"):
            skipped.append({"folder": item["folder"], "uid": item["uid"], "error": "no usable snippet"})
            continue
        needle = item["snippet"].encode("utf-8")
        present = needle in blob
        row = con.execute(
            "select count(*) as n from bodies where folder = ? and uid = ?",
            (item["folder"], item["uid"]),
        ).fetchone()
        has_row = row["n"] > 0
        hits = blob.count(needle)
        results.append(
            {
                "folder": item["folder"],
                "uid": item["uid"],
                "chars": item.get("chars"),
                "snippet": item["snippet"],
                "snippetInFile": present,
                "timesInFile": hits,
                "bodiesRow": has_row,
                "expected": "written" if expect_present else "absent",
                "ok": present == expect_present and has_row == expect_present,
            }
        )
    print(json.dumps({"checked": results, "skipped": skipped}, ensure_ascii=False, indent=2))
    passed = bool(results) and all(item["ok"] for item in results)
    print(
        f"verdict: {'PASS' if passed else 'FAIL'} — {len(results)} checked, "
        f"{sum(1 for item in results if item['ok'])} as expected, {len(skipped)} skipped, "
        f"arm {report.get('phase')}, expected {'written' if expect_present else 'absent'}"
    )
    if not results:
        print("  (no arm contributed a snippet, so nothing was compared)")
    return 0 if passed else 1


if __name__ == "__main__":
    mode = sys.argv[1] if len(sys.argv) > 1 else "plan"
    if mode == "plan":
        plan()
    elif mode == "pair":
        pair(sys.argv[2])
    elif mode == "verify-online":
        sys.exit(verify(sys.argv[2], expect_present=False))
    elif mode == "verify-cached":
        sys.exit(verify(sys.argv[2], expect_present=True))
    else:
        sys.exit(f"unknown mode {mode!r}")
