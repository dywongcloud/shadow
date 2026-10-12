#!/usr/bin/env python3
"""musql migration + row-for-row verification for the managed SQLite lane.

Run on a node that owns managed SQLite databases (owner-routed lane: the files
live on the owner). Stdlib only, no test files, no dependencies.

    scripts/musql-migrate.py migrate     # backup -> convert -> verify
    scripts/musql-migrate.py roundtrip   # export .musq back to sqlite -> verify
    scripts/musql-migrate.py rollback    # musql -> sqlite: export, verify, INSTALL
    scripts/musql-migrate.py verify --sqlite F [--musql-base URL --musql-db HOST]

`migrate` never writes, moves or deletes the original .db: it takes two SQLite
online-backup snapshots, converts the second one to <stem>.musq beside the
original, and reads the result back out of musqld over its Hrana HTTP API.
`rollback` is the other direction and it is the one that touches the LIVE file:
it snapshots the live .db into the backup dir, exports the .musq to a new file,
verifies that export against the engine BEFORE installing it, and only then
moves the live .db (and its -wal/-shm) aside — never deleted — and copies the
verified export into place. Deleting <stem>.musq is NOT a rollback: it leaves
the record pointing at an engine with no file behind it.

Digests are engine independent: every cell is encoded by TYPE (so an int/float
flip cannot pass), rows are canonicalised and SORTED (so row order / rowid
differences cannot pass), then sha256'd.
"""
import argparse, base64, glob, hashlib, json, os, shutil, socket, sqlite3
import struct, subprocess, sys, time, urllib.request

H = hashlib.sha256
USER_TABLES_SQL = ("SELECT name FROM sqlite_master WHERE type='table' "
                   "AND name NOT LIKE 'sqlite_%' ORDER BY name")
SCHEMA_SQL = ("SELECT type, name, tbl_name, sql FROM sqlite_master "
              "WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name")


# ---------------------------------------------------------------- engine-neutral digest
def enc(v):
    if v is None:
        return b"N;"
    if isinstance(v, bool):
        return b"i:%d;" % (1 if v else 0)
    if isinstance(v, int):
        return b"i:%d;" % v
    if isinstance(v, float):
        if v != v:
            return b"f:nan;"          # collapse NaN bit patterns
        return b"f:%s;" % struct.pack(">d", v).hex().encode()
    if isinstance(v, bytes):
        return b"b:%d:%s;" % (len(v), v.hex().encode())
    if isinstance(v, str):
        b = v.encode("utf-8", "surrogatepass")
        return b"s:%d:%s;" % (len(b), b)
    return b"?%s;" % repr(v).encode()


def digest_rows(rows):
    keys = [b"".join(enc(c) for c in r) for r in rows]
    ordered = H()
    for k in keys:
        ordered.update(k); ordered.update(b"\n")
    srt = H()
    for k in sorted(keys):
        srt.update(k); srt.update(b"\n")
    return len(keys), ordered.hexdigest(), srt.hexdigest()


def hrana_value(v):
    t = v.get("type")
    if t == "null":
        return None
    if t == "integer":
        return int(v["value"])
    if t == "float":
        return float(v["value"])
    if t == "text":
        return v["value"]
    if t == "blob":
        b = v.get("base64") or v.get("value") or ""   # Hrana v3: "base64"
        b += "=" * (-len(b) % 4)
        return base64.b64decode(b)
    raise SystemExit("unknown hrana value type %r" % t)


class Musqld:
    def __init__(self, base, dbname):
        self.base, self.host = base.rstrip("/"), dbname

    def query(self, sql):
        body = json.dumps({"baton": None,
                           "requests": [{"type": "execute", "stmt": {"sql": sql}}]}).encode()
        req = urllib.request.Request(self.base + "/v3/pipeline", data=body,
                                     headers={"Content-Type": "application/json",
                                              "Host": self.host})
        with urllib.request.urlopen(req, timeout=120) as r:
            res = json.loads(r.read())
        out = res["results"][0]
        if out.get("type") != "ok":
            raise SystemExit("musql error on %r: %s" % (sql, json.dumps(out)[:400]))
        rr = out["response"]["result"]
        return [tuple(hrana_value(c) for c in row) for row in rr.get("rows") or []]


def side_sqlite(path):
    conn = sqlite3.connect("file:%s?mode=ro" % path, uri=True)
    try:
        out = {"tables": {}, "schema": digest_rows(conn.execute(SCHEMA_SQL).fetchall())[1]}
        for (t,) in conn.execute(USER_TABLES_SQL).fetchall():
            n, d, ds = digest_rows(conn.execute('SELECT * FROM "%s"' % t).fetchall())
            out["tables"][t] = {"rows": n, "digest": ds, "digest_ordered": d}
        return out
    finally:
        conn.close()


def side_musql(base, dbname):
    m = Musqld(base, dbname)
    out = {"tables": {}, "schema": digest_rows(m.query(SCHEMA_SQL))[1]}
    for (t,) in m.query(USER_TABLES_SQL):
        n, d, ds = digest_rows(m.query('SELECT * FROM "%s"' % t))
        out["tables"][t] = {"rows": n, "digest": ds, "digest_ordered": d}
    return out


def compare(before, after):
    rows, ok = [], True
    for t in sorted(set(before["tables"]) | set(after["tables"])):
        b, a = before["tables"].get(t), after["tables"].get(t)
        if b is None or a is None:
            ok = False
            rows.append({"table": t, "verdict": "MISSING_ON_ONE_SIDE",
                         "before_rows": None if b is None else b["rows"],
                         "after_rows": None if a is None else a["rows"]})
            continue
        same_n, same_d = b["rows"] == a["rows"], b["digest"] == a["digest"]
        v = "MATCH" if (same_n and same_d) else ("COUNT_MISMATCH" if not same_n
                                                 else "DIGEST_MISMATCH")
        ok = ok and v == "MATCH"
        rows.append({"table": t, "before_rows": b["rows"], "after_rows": a["rows"],
                     "before_digest": b["digest"], "after_digest": a["digest"],
                     "ordered_identical": b["digest_ordered"] == a["digest_ordered"],
                     "verdict": v})
    return {"tables": rows,
            "before_tables": len(before["tables"]), "after_tables": len(after["tables"]),
            "schema_digest_before": before["schema"], "schema_digest_after": after["schema"],
            "schema_identical": before["schema"] == after["schema"],
            "all_match": bool(ok) and before["schema"] == after["schema"]}


# ---------------------------------------------------------------- node primitives
def sha256(p):
    h = H()
    with open(p, "rb") as f:
        for c in iter(lambda: f.read(1 << 20), b""):
            h.update(c)
    return h.hexdigest()


def snapshot(src, dst):
    """SQLite online backup API: consistent even with a live WAL and writers."""
    s = sqlite3.connect("file:%s?mode=ro" % src, uri=True)
    d = sqlite3.connect(dst)
    try:
        s.backup(d)
    finally:
        d.close(); s.close()


def wait_port(port, host="127.0.0.1"):
    for _ in range(120):
        try:
            socket.create_connection((host, port), timeout=2).close()
            return True
        except OSError:
            time.sleep(0.5)
    return False


def nonfinite(path):
    out = []
    c = sqlite3.connect("file:%s?mode=ro" % path, uri=True)
    try:
        for (t,) in c.execute(USER_TABLES_SQL).fetchall():
            for _, col, _ in c.execute('PRAGMA table_info("%s")' % t):
                try:
                    n = c.execute(
                        'SELECT count(*) FROM "%s" WHERE typeof("%s")=\'real\' '
                        'AND ("%s" != "%s" OR "%s" > 9e307 OR "%s" < -9e307)'
                        % (t, col, col, col, col, col)).fetchone()[0]
                except Exception:
                    continue
                if n:
                    out.append({"table": t, "column": col, "rows": n})
    finally:
        c.close()
    return out


# ---------------------------------------------------------------- subcommands
def cmd_migrate(a):
    d, port = a.dir, a.port
    bk = os.path.join(os.path.dirname(d.rstrip("/")),
                      "sqlite-dbs-backup-%s" % time.strftime("%Y%m%d-%H%M%S"))
    os.makedirs(bk, exist_ok=True)
    files = sorted(f for f in os.listdir(d) if f.startswith("hive-sqlite-") and f.endswith(".db"))
    out = {"node": os.uname().nodename, "dir": d, "backup_dir": bk,
           "files_found": len(files), "databases": []}
    if not files:
        print(json.dumps(out, sort_keys=True)); return 0
    log = open("/tmp/hive-musqld-migrate.log", "ab")
    p = subprocess.Popen([a.musqld, "-dir", d, "-listen", "127.0.0.1:%d" % port,
                          "-idle-timeout", "900s"], stdout=log, stderr=subprocess.STDOUT)
    try:
        if not wait_port(port):
            out["error"] = "musqld did not come up"; print(json.dumps(out, sort_keys=True)); return 1
        base = "http://127.0.0.1:%d" % port
        for f in files:
            stem = f[:-3]
            live = os.path.join(d, f)
            rec = {"file": f, "db_id": stem[len("hive-sqlite-"):],
                   "live_db_bytes": os.path.getsize(live), "live_db_sha256": sha256(live)}
            wal = live + "-wal"
            rec["live_wal_bytes"] = os.path.getsize(wal) if os.path.exists(wal) else 0
            rec["live_wal_sha256"] = sha256(wal) if os.path.exists(wal) else None
            s1, s2 = os.path.join(bk, stem + ".db"), os.path.join(bk, stem + ".snap2.db")
            snapshot(live, s1); time.sleep(0.2); snapshot(live, s2)
            rec.update(backup_s1=s1, backup_s1_bytes=os.path.getsize(s1),
                       backup_s1_sha256=sha256(s1),
                       backup_s2=s2, backup_s2_bytes=os.path.getsize(s2),
                       backup_s2_sha256=sha256(s2))
            before = side_sqlite(s2)
            rec["before"] = before
            rec["s1_equals_s2"] = compare(side_sqlite(s1), before)["all_match"] is True
            rec["nonfinite_real"] = nonfinite(s2)
            musq = os.path.join(d, stem + ".musq")
            r = subprocess.run([a.convert, "import", "-force", s2, musq],
                               capture_output=True, text=True)
            rec.update(convert_exit=r.returncode, convert_stderr=(r.stderr or "").strip()[:400],
                       musq_path=musq,
                       musq_bytes=os.path.getsize(musq) if os.path.exists(musq) else None,
                       musq_sha256=sha256(musq) if os.path.exists(musq) else None)
            after = side_musql(base, stem + ".localhost")
            rec["after"] = after
            rec["compare"] = compare(before, after)
            rec["all_match"] = rec["compare"]["all_match"]
            out["databases"].append(rec)
    finally:
        p.terminate()
        try:
            p.wait(timeout=15)
        except Exception:
            p.kill()
    print(json.dumps(out, sort_keys=True))
    return 0 if all(r.get("all_match") for r in out["databases"]) else 2


def cmd_roundtrip(a):
    d = a.dir
    bks = sorted(glob.glob(os.path.join(os.path.dirname(d.rstrip("/")), "sqlite-dbs-backup-*")))
    bk = bks[-1] if bks else None
    work = "/tmp/hive-musql-roundtrip"
    shutil.rmtree(work, ignore_errors=True); os.makedirs(work)
    iso = os.path.join(work, "isolated"); os.makedirs(iso)
    rep = {"node": os.uname().nodename, "backup_dir": bk, "databases": []}
    for musq in sorted(glob.glob(os.path.join(d, "hive-sqlite-*.musq"))):
        stem = os.path.basename(musq)[:-5]
        s2 = os.path.join(bk, stem + ".snap2.db") if bk else None
        rec = {"db_id": stem[len("hive-sqlite-"):], "musq_bytes": os.path.getsize(musq),
               "musq_sha256": sha256(musq)}
        exp = os.path.join(work, stem + ".export.db")
        r = subprocess.run([a.convert, "export", musq, exp], capture_output=True, text=True)
        rec["export_exit"] = r.returncode
        rec["export_stderr"] = (r.stderr or "").strip()[:300]
        if r.returncode == 0 and os.path.exists(exp):
            cmp = compare(side_sqlite(s2), side_sqlite(exp))
            rec["roundtrip_vs_backup"] = cmp
            rec["roundtrip_match"] = cmp["all_match"]
        shutil.copy2(musq, iso)          # .musq alone, no .delta/.lock/.db
        rep["databases"].append(rec)
    p = subprocess.Popen([a.musqld, "-dir", iso, "-listen", "127.0.0.1:%d" % a.port,
                          "-idle-timeout", "900s"],
                         stdout=open(os.path.join(work, "musqld.log"), "ab"),
                         stderr=subprocess.STDOUT)
    try:
        wait_port(a.port)
        base = "http://127.0.0.1:%d" % a.port
        for rec in rep["databases"]:
            stem = "hive-sqlite-" + rec["db_id"]
            try:
                cmp = compare(side_sqlite(os.path.join(bk, stem + ".snap2.db")),
                              side_musql(base, stem + ".localhost"))
                rec["isolated_match"] = cmp["all_match"]
                rec["isolated_tables"] = cmp["after_tables"]
            except Exception as e:
                rec["isolated_match"] = False
                rec["isolated_error"] = repr(e)[:200]
    finally:
        p.terminate()
        try:
            p.wait(timeout=15)
        except Exception:
            p.kill()
    print(json.dumps(rep, sort_keys=True))
    return 0 if all(r.get("isolated_match") and r.get("roundtrip_match")
                    for r in rep["databases"]) else 2


def flip(base, token, db_id, engine):
    """Flip the database RECORD to `engine` through the operator endpoint."""
    body = json.dumps({"engine": engine}).encode()
    req = urllib.request.Request("%s/v1/databases/%s/sqlite-engine" % (base.rstrip("/"), db_id),
                                 data=body, method="POST",
                                 headers={"Content-Type": "application/json",
                                          "Authorization": "Bearer %s" % token})
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            return {"status": r.status, "body": json.loads(r.read())}
    except Exception as e:                      # never echo the token
        return {"status": None, "error": repr(e).split("Bearer")[0][:200]}


def cmd_rollback(a):
    """musql -> sqlite for real: export, verify, then install.

    The install is the part that has only ever been written, never run, so it is
    ordered the same way as the Rust rollback: snapshot the live file, export to
    a NEW file, verify that export against the engine, and only then move the
    live file aside (never delete it) and copy the verified export into place.
    """
    d, port = a.dir, a.port
    bk = os.path.join(os.path.dirname(d.rstrip("/")),
                      "sqlite-dbs-rollback-%s" % time.strftime("%Y%m%d-%H%M%S"))
    os.makedirs(bk, exist_ok=True)
    out = {"node": os.uname().nodename, "dir": d, "backup_dir": bk, "databases": []}
    log = open("/tmp/hive-musqld-rollback.log", "ab")
    p = subprocess.Popen([a.musqld, "-dir", d, "-listen", "127.0.0.1:%d" % port,
                          "-idle-timeout", "900s"], stdout=log, stderr=subprocess.STDOUT)
    try:
        if not wait_port(port):
            out["error"] = "musqld did not come up"
            print(json.dumps(out, sort_keys=True))
            return 1
        base = "http://127.0.0.1:%d" % port
        for musq in sorted(glob.glob(os.path.join(d, "hive-sqlite-*.musq"))):
            stem = os.path.basename(musq)[:-5]
            db_id = stem[len("hive-sqlite-"):]
            if a.db_id and db_id not in a.db_id:
                continue
            live = os.path.join(d, stem + ".db")
            rec = {"db_id": db_id, "musq_bytes": os.path.getsize(musq), "musq_sha256": sha256(musq)}
            if os.path.exists(live):
                # Snapshot first: the live file is about to be moved aside, and a
                # crash between the two steps must still leave the pre-rollback
                # database on disk.
                pre = os.path.join(bk, stem + ".pre-rollback.db")
                snapshot(live, pre)
                rec["pre_rollback_snapshot"] = pre
                rec["pre_rollback_sha256"] = sha256(pre)
            exp = os.path.join(bk, stem + ".rollback.db")
            r = subprocess.run([a.convert, "export", musq, exp], capture_output=True, text=True)
            rec["export_exit"] = r.returncode
            rec["export_stderr"] = (r.stderr or "").strip()[:300]
            if r.returncode != 0 or not os.path.exists(exp):
                rec["verdict"] = "EXPORT_FAILED"
                out["databases"].append(rec)
                continue
            # Verify BEFORE it becomes the live file.
            rec["verify_vs_musql"] = compare(side_musql(base, stem + ".localhost"),
                                             side_sqlite(exp))
            rec["verified_before_install"] = rec["verify_vs_musql"]["all_match"]
            if not rec["verified_before_install"]:
                rec["verdict"] = "REFUSED_UNVERIFIED"
                out["databases"].append(rec)
                continue
            if a.dry_run:
                rec["verdict"] = "DRY_RUN_VERIFIED"
                out["databases"].append(rec)
                continue
            if os.path.exists(live):
                aside = os.path.join(bk, stem + ".installed-over.db")
                os.rename(live, aside)
                rec["live_moved_aside"] = aside
            for suf in ("-wal", "-shm"):
                f = live + suf
                if os.path.exists(f):
                    os.rename(f, os.path.join(bk, stem + ".installed-over" + suf))
                    rec.setdefault("sidecars_moved_aside", []).append(suf)
            shutil.copy2(exp, live)
            rec["installed_bytes"] = os.path.getsize(live)
            rec["installed_sha256"] = sha256(live)
            # Prove it: what an app opens must be what the engine still serves.
            rec["post_install"] = compare(side_musql(base, stem + ".localhost"),
                                          side_sqlite(live))
            rec["app_data_intact"] = rec["post_install"]["all_match"]
            rec["verdict"] = "ROLLED_BACK" if rec["app_data_intact"] else "POST_INSTALL_DIVERGENT"
            if a.api_base and a.token:
                rec["record_flip"] = flip(a.api_base, a.token, db_id, "rusqlite")
            else:
                rec["record_flip_cmd"] = (
                    "curl -s -X POST -H 'authorization: bearer \"$OPS_TOKEN\"' "
                    "-H 'content-type: application/json' -d '{\"engine\":\"rusqlite\"}' "
                    "%s/v1/databases/%s/sqlite-engine"
                    % (a.api_base or "https://api.<platform-domain>", db_id))
            out["databases"].append(rec)
    finally:
        p.terminate()
        try:
            p.wait(timeout=15)
        except Exception:
            p.kill()
    ok = {"ROLLED_BACK", "DRY_RUN_VERIFIED"}
    print(json.dumps(out, sort_keys=True))
    return 0 if all(r.get("verdict") in ok for r in out["databases"]) else 2


def cmd_verify(a):
    before = side_sqlite(a.sqlite)
    if a.musql_base and a.musql_db:
        rep = compare(before, side_musql(a.musql_base, a.musql_db))
    else:
        rep = {"tables": [{"table": t, "before_rows": v["rows"],
                           "before_digest": v["digest"], "verdict": "BEFORE_ONLY"}
                          for t, v in sorted(before["tables"].items())],
               "schema_digest_before": before["schema"], "all_match": None}
    rep["sqlite"] = a.sqlite
    print(json.dumps(rep, sort_keys=True))
    return 0


def main():
    # Shared options live on EVERY subcommand (via `parents`), not on the main
    # parser: argparse resolves a subparser default over a value the main parser
    # already parsed, so declaring them in both places would silently discard
    # `--dir` given before the subcommand name.
    common = argparse.ArgumentParser(add_help=False)
    common.add_argument("--dir", default=os.path.join(
        os.environ.get("HIVE_DATA", "/var/lib/hive/data"), "sqlite-dbs"))
    common.add_argument("--musqld", default=os.environ.get("HIVE_MUSQLD_BIN", "/usr/local/bin/musqld"))
    common.add_argument("--convert", default=os.environ.get("HIVE_MUSQL_CONVERT_BIN",
                                                            "/usr/local/bin/musql-convert"))
    common.add_argument("--port", type=int, default=18699)

    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)

    sub.add_parser("migrate", parents=[common]).set_defaults(fn=cmd_migrate)
    sub.add_parser("roundtrip", parents=[common]).set_defaults(fn=cmd_roundtrip)
    rb = sub.add_parser("rollback", parents=[common])
    rb.add_argument("--db-id", action="append", help="only these database ids (repeatable)")
    rb.add_argument("--dry-run", action="store_true",
                    help="export and verify, but do not touch the live file")
    rb.add_argument("--api-base", help="flip the record too, e.g. https://api.<platform>")
    rb.add_argument("--token", default=os.environ.get("HIVE_OPS_TOKEN", ""),
                    help="operator bearer for --api-base (default $HIVE_OPS_TOKEN)")
    rb.set_defaults(fn=cmd_rollback)
    v = sub.add_parser("verify", parents=[common])
    v.add_argument("--sqlite", required=True)
    v.add_argument("--musql-base")
    v.add_argument("--musql-db")
    v.set_defaults(fn=cmd_verify)
    a = ap.parse_args()
    sys.exit(a.fn(a) or 0)


if __name__ == "__main__":
    main()
