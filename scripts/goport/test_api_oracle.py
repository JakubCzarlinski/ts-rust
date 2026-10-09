#!/usr/bin/env python3
"""Unit tests for api_oracle.py protocol 3 (pin B) and protocol 4 (pin N). No server runs.

  python3 scripts/goport/test_api_oracle.py
"""
import importlib.util
import os
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))


def load(pin):
    """api_oracle.py imported as a fresh module with GOPORT_PIN_ACTIVE=pin (PROTOCOL is fixed at import)."""
    old = os.environ.get("GOPORT_PIN_ACTIVE")
    os.environ["GOPORT_PIN_ACTIVE"] = pin
    try:
        spec = importlib.util.spec_from_file_location(f"api_oracle_{pin}", os.path.join(HERE, "api_oracle.py"))
        mod = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(mod)
        return mod
    finally:
        if old is None:
            del os.environ["GOPORT_PIN_ACTIVE"]
        else:
            os.environ["GOPORT_PIN_ACTIVE"] = old


B, N = load("16c25522e123"), load("673a5f17d713")
OPEN = {"openProjects": ["@PROJECT_DIR@/tsconfig.json"]}
CHANGE = {"fileChanges": {"changed": ["@PROJECT_DIR@/a.ts"]}}
TEMP = {"file": "@PROJECT_DIR@/a.ts", "newText": "x"}
PF = {"event": 1, "pointer": "/snapshot", "into": "/snapshot"}


def events(mod):
    """The snapshot events of every builder form: stdio, LSP and temporary."""
    tb, lsp = mod.TraceBuilder(), mod.TraceBuilder()
    for b in (OPEN, {}, CHANGE, OPEN):
        tb.snap(b)
    tb.temp(TEMP, PF)
    tb.temp({"snapshot": 999, **TEMP}, None)
    for b in ({}, OPEN, {}):
        lsp.snap(b, lsp=True)
    return tb.events + lsp.events


def run(mod, wire=None):
    r = mod.SessionRun({"project": {"dir": "/p", "tsconfig": "tsconfig.json"}}, [], "tsgo", "oracle", "/tmp",
                       wire=wire)
    r.ctx = {"snapshot": None, "projects": []}
    return r


def proj(pid, n=0):
    return {"id": pid, "configFileName": pid, "n": n}


class Protocol(unittest.TestCase):
    def test_protocols(self):
        self.assertEqual((B.PROTOCOL, N.PROTOCOL), (3, 4))

    def test_b_events_unchanged(self):
        ev = events(B)
        self.assertEqual([e["method"] for e in ev], ["updateSnapshot"] * 4 + ["updateTemporarySnapshot"] * 2
                         + ["updateSnapshot"] * 3)
        self.assertEqual([e["params"] for e in ev], [OPEN, {}, CHANGE, OPEN, TEMP, {"snapshot": 999, **TEMP},
                                                     {}, OPEN, {}])
        self.assertEqual(ev[4]["paramsFrom"], PF)
        self.assertFalse(any("track" in e for e in ev))

    def test_n_events(self):
        ev = events(N)
        s, fn = "@SNAPSHOT@", {"fileNotifications": CHANGE["fileChanges"], "ensurePrograms": True}
        layer = {"fileSystem": {"kind": "layer", "files": {TEMP["file"]: "x"}}, "ensurePrograms": True}
        self.assertEqual([(e["method"], e["params"]) for e in ev], [
            ("createSnapshot", OPEN), ("updateSnapshot", {"snapshot": s}),
            ("updateSnapshot", {"snapshot": s, "changes": fn}), ("updateSnapshot", {"snapshot": s, "changes": OPEN}),
            ("updateSnapshot", {"changes": layer}), ("updateSnapshot", {"snapshot": 999, "changes": layer}),
            ("getCurrentLanguageServerSnapshot", {}),
            ("getCurrentLanguageServerSnapshot", {"baseSnapshot": s, "changes": OPEN}),
            ("getCurrentLanguageServerSnapshot", {"baseSnapshot": s})])
        self.assertEqual([e.get("track") for e in ev][4:6], [False, False])
        self.assertEqual(ev[4]["paramsFrom"], PF)

    def test_wire3_inverts(self):
        self.assertEqual([N.wire3_event(e) for e in events(N)], events(B))
        self.assertEqual(N.wire3("getSemanticDiagnostics", {"x": 1}), ("getSemanticDiagnostics", {"x": 1}))

    def test_expand_keys(self):
        ctx = {"snapshot": 7, "project_dir": "/p", "run_dir": "/r", "tsconfig": "tsconfig.json", "projects": []}
        got = N.expand({"snapshot": "@SNAPSHOT@", "files": {"@PROJECT_DIR@/a.ts": "@RUN_DIR@"}}, ctx)
        self.assertEqual(got, {"snapshot": 7, "files": {"/p/a.ts": "/r"}})

    def test_track(self):
        a, b, c = proj("a"), proj("b"), proj("c")
        r = run(N)
        r._track({"method": "createSnapshot"}, OPEN, {"snapshot": 1, "projects": [a, b]})
        r._track({"method": "updateSnapshot"}, {}, {"snapshot": 2, "projects": [proj("a", 1), c],
                                                    "changes": {"removedProjects": ["b"]}})
        self.assertEqual(r.ctx, {"snapshot": 2, "projects": [proj("a", 1), c]})
        r._track({"method": "updateSnapshot", "track": False}, {}, {"snapshot": 3, "projects": []})
        r._track({"method": "getCurrentLanguageServerSnapshot"}, {}, {"snapshot": 4, "projects": [b]})
        self.assertEqual(r.ctx, {"snapshot": 4, "projects": [b]})
        r._track({"method": "getCurrentLanguageServerSnapshot"}, {"baseSnapshot": 4}, {"snapshot": 5, "projects": []})
        self.assertEqual(r.ctx, {"snapshot": 5, "projects": [b]})
        for mod, wire in ((B, None), (N, 3)):  # protocol 3: each updateSnapshot answer replaces, nothing else
            r = run(mod, wire)
            r._track({"method": "updateSnapshot"}, {}, {"snapshot": 1, "projects": [a, b]})
            r._track({"method": "updateTemporarySnapshot"}, {}, {"snapshot": 2, "projects": [c]})
            r._track({"method": "updateSnapshot"}, {}, {"snapshot": 3, "projects": [c]})
            self.assertEqual(r.ctx, {"snapshot": 3, "projects": [c]})

    def test_wire_events(self):
        self.assertEqual(run(N, 3).events, [])
        r = N.SessionRun({}, events(N), "tsgo", "oracle", "/tmp", wire=3)
        self.assertEqual(r.events, events(B))

    def test_prepare_sorts_create(self):
        ans = {"changes": {"changedProjects": {"p": {"changedFiles": ["b", "a"]}}, "removedProjects": ["y", "x"]}}
        got = N.Normalizer([]).prepare("createSnapshot", ans)["changes"]
        self.assertEqual(got, {"changedProjects": {"p": {"changedFiles": ["a", "b"]}}, "removedProjects": ["x", "y"]})


if __name__ == "__main__":
    unittest.main()
