#!/usr/bin/env python3
"""The acceptance run's probe: wait, push, query a store, read one trace.

Run by `scripts/obs-acceptance.sh`; kept beside it so the run itself stays
shell and the one real program in it stays reviewable.
"""

import json
import sys
import time
import urllib.error
import urllib.parse
import urllib.request


def get(url, timeout=10):
    with urllib.request.urlopen(url, timeout=timeout) as r:
        return json.load(r)


def post(url, body, token, timeout=10):
    req = urllib.request.Request(
        url,
        data=json.dumps(body).encode(),
        headers={"Content-Type": "application/json", "Authorization": "Bearer " + token},
        method="POST",
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status
    except urllib.error.HTTPError as e:
        return e.code


def span(trace_id, claim):
    now = time.time_ns()
    return {"resourceSpans": [{
        "resource": {"attributes": [
            {"key": "service.name", "value": {"stringValue": "acceptance"}},
            {"key": "tagma.id", "value": {"stringValue": claim}},
        ]},
        "scopeSpans": [{"scope": {"name": "obs-acceptance"}, "spans": [{
            "traceId": trace_id, "spanId": "eee19b7ec3c1b174", "name": "acceptance",
            "kind": 1, "startTimeUnixNano": str(now), "endTimeUnixNano": str(now + 1000),
        }]}],
    }]}


def metric(name, claim):
    now = time.time_ns()
    return {"resourceMetrics": [{
        "resource": {"attributes": [
            {"key": "service.name", "value": {"stringValue": "acceptance"}},
            {"key": "tagma.id", "value": {"stringValue": claim}},
        ]},
        "scopeMetrics": [{"scope": {"name": "obs-acceptance"}, "metrics": [{
            "name": name, "gauge": {"dataPoints": [{"asDouble": 1.0, "timeUnixNano": str(now)}]},
        }]}],
    }]}


def main():
    command = sys.argv[1]
    if command == "wait":
        url, secs = sys.argv[2], int(sys.argv[3])
        deadline = time.time() + secs
        while time.time() < deadline:
            try:
                with urllib.request.urlopen(url, timeout=3):
                    print("ready")
                    return 0
            except Exception:
                time.sleep(1)
        print("timeout")
        return 1
    if command == "push":
        door, token, kind, claim = sys.argv[2], sys.argv[3], sys.argv[4], sys.argv[5]
        trace_id = ("%032x" % time.time_ns())[:32]
        body = span(trace_id, claim) if kind == "trace" else metric("acceptance_probe_" + trace_id[:8], claim)
        path = "/v1/traces" if kind == "trace" else "/v1/metrics"
        status = post("http://127.0.0.1:%s%s" % (door, path), body, token)
        print(status, trace_id)
        return 0
    if command == "prom":
        base, expr = sys.argv[2], sys.argv[3]
        data = get("%s/api/v1/query?query=%s" % (base, urllib.parse.quote(expr)))
        for row in data["data"]["result"]:
            labels = ",".join("%s=%s" % (k, v) for k, v in sorted(row["metric"].items()) if k != "__name__")
            print("%s %s" % (labels, row["value"][1]))
        return 0
    if command == "tempo-trace":
        # One trace by id: a search over a busy process's own traces answers a
        # capped page, which is no way to look for a fresh one.
        base, trace_id = sys.argv[2], sys.argv[3]
        detail = get("%s/api/traces/%s" % (base, trace_id))
        attrs = detail["batches"][0]["resource"]["attributes"]
        found = {a["key"]: a["value"].get("stringValue", "") for a in attrs}
        print(found.get("tagma.id", "-"))
        return 0
    if command == "tempo-count":
        # How many traces the store holds under one resource attribute value.
        base = sys.argv[2]
        expr = sys.argv[3]
        search = get(base + "/api/search?limit=100&q=" + urllib.parse.quote(expr))
        print(len(search.get("traces", [])))
        return 0
    if command == "tempo":
        base = sys.argv[2]
        search = get(base + "/api/search?limit=100")
        rows = []
        for trace in search.get("traces", []):
            trace_id = trace.get("traceID") or trace.get("traceId")
            try:
                detail = get("%s/api/traces/%s" % (base, trace_id))
            except urllib.error.HTTPError:
                continue
            attrs = detail["batches"][0]["resource"]["attributes"]
            found = {a["key"]: a["value"].get("stringValue", "") for a in attrs}
            rows.append((
                int(trace.get("startTimeUnixNano", 0)),
                found.get("service.instance.id", "-"),
                found.get("tagma.id", "-"),
                trace_id,
            ))
        for row in sorted(rows):
            print("%d %s %s %s" % row)
        return 0
    raise SystemExit("obs-acceptance: unknown probe command %s" % command)


sys.exit(main())
