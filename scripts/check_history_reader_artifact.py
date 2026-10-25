#!/usr/bin/env python3
"""Exercise the actual CI executable on authored, isolated PostgreSQL history only."""
import argparse
from decimal import Decimal
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
from urllib.parse import unquote, urlparse

from history_reader_artifact import require

PREFIX = "ic_history_artifact_"
TABLE = PREFIX + "runs"
FIELDS = ["prompt_tokens", "completion_tokens", "total_tokens", "cached_tokens",
          "cache_write_tokens", "reasoning_tokens"]


def snapshot(unknown):
    return {
        "budget": dict(state="unavailable" if unknown else "disabled", limit=None,
                       charged="0", retained="0", reserved="0", in_flight="0"),
        "in_flight": "0", "coverage": "unavailable" if unknown else "complete",
        "settled": dict(requests="0", coverage="unavailable" if unknown else "complete",
                        **{field: dict(known=None if unknown else "0", complete=not unknown)
                           for field in FIELDS}),
    }


def fixtures():
    legacy = dict(task="summary", agent="author", output="authored offline fixture",
                  success=True, duration_ms=1,
                  token_usage=dict(total_tokens=123456789012345678901234567890,
                                   decimal="__exact_decimal__"))
    current = {key: value for key, value in legacy.items() if key != "token_usage"}
    current["usage"] = snapshot(False)
    bad = dict(legacy, usage=None)
    return [("legacy", "", [legacy], snapshot(True)),
            ("current", "retired-flow", [current], snapshot(False)),
            ("mixed", "", [legacy, current], snapshot(True)),
            ("invalid", "retired-flow", [bad], snapshot(True))]


def database_url(name):
    value = os.environ.get(name, "")
    url = urlparse(value)
    require(url.scheme in ["postgres", "postgresql"] and url.hostname in ["localhost", "127.0.0.1"]
            and url.path == "/history_fixture", "Only the isolated history_fixture database is allowed")
    require(not url.query and not url.fragment, "Fixture URL options are prohibited")
    return value, url


def qualify(binary):
    admin, admin_url = database_url("HISTORY_FIXTURE_ADMIN_URL")
    reader, reader_url = database_url("DATABASE_URL")
    require(admin_url.hostname == reader_url.hostname and admin_url.port == reader_url.port,
            "Fixture connections must use the same isolated server")
    require(admin_url.username == "postgres" and reader_url.username == "history_reader"
            and reader_url.password == "history_fixture_password", "Unexpected fixture role")

    def sql(query, url=admin, success=True):
        connection = urlparse(url)
        env = dict(PATH=os.environ["PATH"], PGHOST=connection.hostname,
                   PGPORT=str(connection.port or 5432), PGUSER=unquote(connection.username),
                   PGPASSWORD=unquote(connection.password or ""), PGDATABASE="history_fixture",
                   PGAPPNAME="ic-history-artifact-fixture")
        result = subprocess.run(["psql", "-X", "-qAt", "--no-password", "-v", "ON_ERROR_STOP=1"],
                                input=query, env=env, text=True, capture_output=True, timeout=30)
        require((result.returncode == 0) == success, "Isolated fixture SQL assertion failed")
        return result.stdout.strip()

    sql(f"""CREATE TABLE {TABLE} (run_id text PRIMARY KEY, flow_name text, flow text, status text,
        started_at text, finished_at text, duration_ms bigint, agent_count int, task_count int,
        task_results jsonb, usage jsonb);
        CREATE ROLE history_reader LOGIN PASSWORD 'history_fixture_password'
          NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT NOREPLICATION NOBYPASSRLS;
        GRANT CONNECT ON DATABASE history_fixture TO history_reader;
        GRANT USAGE ON SCHEMA public TO history_reader;
        GRANT SELECT ON {TABLE} TO history_reader;
        ALTER ROLE history_reader SET default_transaction_read_only=on;""")
    try:
        for identifier, flow, tasks, usage in fixtures():
            raw_tasks = json.dumps(tasks).replace('"__exact_decimal__"',
                                                  "0.123456789012345678901234567890")
            sql(f"""INSERT INTO {TABLE} VALUES ('{identifier}', 'authored goal', '{flow}', 'success',
                '2026-01-01T00:00:00Z','2026-01-01T00:00:01Z',1,1,{len(tasks)},
                $json${raw_tasks}$json$::jsonb,$json${json.dumps(usage)}$json$::jsonb);""")
        query = f"SELECT row_to_json(r)::text FROM {TABLE} r ORDER BY run_id;"
        before = sql(query)
        originals = {row["run_id"]: row for row in
                     [json.loads(raw, parse_float=Decimal) for raw in before.splitlines()]}
        schema_query = f"SELECT column_name,data_type FROM information_schema.columns WHERE table_name='{TABLE}' ORDER BY ordinal_position;"
        schema = sql(schema_query)
        sql(f"INSERT INTO {TABLE}(run_id) VALUES('forbidden');", reader, success=False)
        with tempfile.TemporaryDirectory(prefix="ic-history-artifact-") as directory:
            cwd = Path(directory)
            # A poisoned ancestor file must never affect this operator command.
            (cwd / ".env").write_text("DATABASE_URL=private-fixture-sentinel\n")
            nested = cwd / "nested"
            nested.mkdir()
            env = dict(PATH=os.environ["PATH"], DATABASE_URL=reader,
                       IRONCREW_PG_TABLE_PREFIX=PREFIX, IRONCREW_LOG="error")

            def cli(*args, success=True, explicit=True):
                selected = dict(env)
                if not explicit:
                    selected.pop("DATABASE_URL")
                result = subprocess.run([str(binary), "history", *args], cwd=nested, env=selected,
                                        text=True, capture_output=True, timeout=30)
                require((result.returncode == 0) == success, "History executable assertion failed")
                if success:
                    return json.loads(result.stdout, parse_float=Decimal)
                require("private-fixture-sentinel" not in result.stdout + result.stderr,
                        "History error exposed fixture configuration")
                return result.stdout + result.stderr

            seen = []
            for offset in [0, 2]:
                page = cli("list", "--limit", "2", "--offset", str(offset))
                require(page["read_only"] and page["has_more"] == (offset == 0), "Invalid page")
                seen.extend(row["run_id"] for row in page["runs"])
            require(set(seen) == set(originals) and len(seen) == 4, "Incomplete history pagination")
            unknown = 0
            for identifier in ["legacy", "current", "mixed"]:
                view = cli("inspect", identifier)
                require(view["read_only"] and not view["executable"], "Executable history view")
                require(view["record"] == originals[identifier], "Original history precision changed")
                require(view["run_usage"] == originals[identifier]["usage"], "Run usage changed")
                for projected, original in zip(view["tasks"], originals[identifier]["task_results"], strict=True):
                    require(projected["usage"] == original.get("usage"), "Task usage changed")
                    unknown += int(projected["usage"] is None)
            require(unknown == 2, "Legacy unknown usage became a receipt")
            require(len(cli("list", "--flow", "")["runs"]) == 2, "Empty scope changed")
            cli("inspect", "invalid", success=False)
            cli("inspect", "missing", success=False)
            cli("inspect", "legacy", "--flow", "retired-flow", success=False)
            cli("list", "--table-prefix", "bad;prefix", success=False)
            require("explicit DATABASE_URL" in cli("list", success=False, explicit=False),
                    "Ancestor dotenv was used")
            require(not list(cwd.rglob(".ironcrew")), "Runtime state was created")
        require(sql(query) == before and sql(schema_query) == schema, "History/schema mutated")
        return dict(read_only=True, original_rows_unchanged=4, valid_rows_inspected=3,
                    unknown_task_usage_preserved=2, checked_zero_preserved=True,
                    precision_preserved=True, malformed_usage_rejected=True, select_only=True,
                    ancestor_dotenv_ignored=True, schema_unchanged=True, provider_calls=0,
                    database_sha256=hashlib.sha256(before.encode()).hexdigest(),
                    postgres_version=sql("SHOW server_version;"))
    finally:
        sql(f"DROP TABLE {TABLE}; DROP OWNED BY history_reader; DROP ROLE history_reader;")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    receipt = qualify(args.binary.resolve())
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(receipt, sort_keys=True, indent=2) + "\n")
    print(json.dumps(receipt, sort_keys=True))


if __name__ == "__main__":
    main()
