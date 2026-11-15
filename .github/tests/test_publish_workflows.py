"""Tests for the shell in the release workflows.

Covers publish.yml, tag-on-merge.yml, crates-io-drift.yml,
crates-io-drift-alert.yml and .github/scripts/check-release-version.sh. The
real `run:` blocks are taken from the workflow YAML and run with bash, the way
GitHub runs them, against:
  - throwaway git repositories (each with a bare `origin`);
  - a mock crates.io (sparse index and API) and GitHub API on 127.0.0.1;
  - stub `cargo` and `gh` commands that record their arguments. The stub
    `cargo publish` "uploads" by appending the version to the mock index, and
    `cargo metadata` prints the checked-out tree's `metadata.json`.
No network access and no credentials are needed. Needs python3 (stdlib only),
bash, git and curl.

Usage: python3 -I .github/tests/test_publish_workflows.py <repo-root>
"""
import glob
import http.server
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import threading
import time

REPO = os.path.abspath(sys.argv[1] if len(sys.argv) > 1 else ".")
WF = os.path.join(REPO, ".github", "workflows")
CHECK_SCRIPT = os.path.join(REPO, ".github", "scripts", "check-release-version.sh")


# ---- a YAML subset loader ---------------------------------------------------
# Enough YAML for these workflow files (block mappings and sequences, plain and
# quoted scalars, flow sequences, `|` / `>` block scalars), with PyYAML's YAML
# 1.1 scalar types (so `on:` is the key True, as in PyYAML). Anything else is
# an error rather than a guess. When PyYAML is installed the loader is checked
# against it on every workflow file.
class YamlError(Exception):
    pass


_BOOL = {"true": True, "false": False, "yes": True, "no": False, "on": True, "off": False}
_INT = re.compile(r"^[-+]?(0|[1-9][0-9_]*)$")
_FLOAT = re.compile(r"^[-+]?([0-9][0-9_]*)?\.[0-9_]*([eE][-+][0-9]+)?$")


def _resolve(text):
    if text in ("", "~", "null", "Null", "NULL"):
        return None
    if text.lower() in _BOOL and text in (text.lower(), text.upper(), text.capitalize()):
        return _BOOL[text.lower()]
    if _INT.match(text):
        return int(text.replace("_", ""))
    if _FLOAT.match(text) and text not in (".", "-.", "+."):
        return float(text.replace("_", ""))
    return text


def _quoted(text, i):
    """Parse a quoted scalar starting at text[i]; return (value, end index)."""
    q = text[i]
    out = []
    j = i + 1
    while j < len(text):
        c = text[j]
        if q == "'" and c == "'":
            if text[j + 1:j + 2] == "'":
                out.append("'")
                j += 2
                continue
            return "".join(out), j + 1
        if q == '"' and c == "\\":
            esc = text[j + 1:j + 2]
            table = {'"': '"', "\\": "\\", "n": "\n", "t": "\t", "/": "/"}
            if esc not in table:
                raise YamlError(f"unsupported escape \\{esc}")
            out.append(table[esc])
            j += 2
            continue
        if q == '"' and c == '"':
            return "".join(out), j + 1
        out.append(c)
        j += 1
    raise YamlError(f"unterminated quote: {text}")


def _strip_comment(text):
    """Drop a trailing ` # comment` outside quotes."""
    i, n = 0, len(text)
    while i < n:
        c = text[i]
        if c in "'\"" and (i == 0 or text[i - 1] in " [,:"):
            _, i = _quoted(text, i)
            continue
        if c == "#" and (i == 0 or text[i - 1] in " \t"):
            return text[:i].rstrip()
        i += 1
    return text.rstrip()


def _scalar(text):
    text = _strip_comment(text.strip())
    if not text:
        return None
    if text[0] in "'\"":
        value, end = _quoted(text, 0)
        if text[end:].strip():
            raise YamlError(f"text after a quoted scalar: {text}")
        return value
    if text == "{}":
        return {}
    if text[0] == "[":
        if not text.endswith("]"):
            raise YamlError(f"multi-line flow sequence: {text}")
        inner, items, i = text[1:-1], [], 0
        while i < len(inner):
            while i < len(inner) and inner[i] in " ,":
                i += 1
            if i >= len(inner):
                break
            if inner[i] in "'\"":
                v, i = _quoted(inner, i)
                items.append(v)
            else:
                j = inner.find(",", i)
                j = len(inner) if j < 0 else j
                items.append(_resolve(inner[i:j].strip()))
                i = j
        return items
    if text[0] in "{&*!|>%@`":
        raise YamlError(f"unsupported YAML: {text}")
    return _resolve(text)


def _key_split(text):
    """Split `key: rest` (key possibly quoted); None when the line is no key."""
    if text[0] in "'\"":
        key, end = _quoted(text, 0)
        rest = text[end:]
        if not rest.startswith(":"):
            return None
        return key, rest[1:]
    m = re.match(r"^([^#'\"{}\[\]][^:]*?):(\s|$)", text)
    if not m:
        return None
    return _resolve(m.group(1).strip()), text[m.end(1) + 1:]


class _Loader:
    def __init__(self, src):
        self.lines = src.split("\n")
        self.i = 0

    def meaningful(self):
        """Index of the next non-blank, non-comment line (or None)."""
        j = self.i
        while j < len(self.lines):
            s = self.lines[j].strip()
            if s and not s.startswith("#"):
                return j
            j += 1
        return None

    @staticmethod
    def _indent(line):
        return len(line) - len(line.lstrip(" "))

    @staticmethod
    def _is_item(text):
        return text == "-" or text.startswith("- ")

    def node(self, min_indent):
        j = self.meaningful()
        if j is None or self._indent(self.lines[j]) < min_indent:
            return None
        self.i = j
        line = self.lines[j]
        if self._is_item(line.strip()):
            return self.seq(self._indent(line))
        return self.mapping(self._indent(line))

    def mapping(self, ind):
        out = {}
        while True:
            j = self.meaningful()
            if j is None:
                return out
            line = self.lines[j]
            if self._indent(line) < ind:
                return out
            if self._indent(line) > ind:
                raise YamlError(f"line {j + 1}: unexpected indentation (multi-line plain scalar?)")
            text = line.strip()
            if self._is_item(text):
                return out
            kv = _key_split(text)
            if kv is None:
                raise YamlError(f"line {j + 1}: not a key: {text}")
            key, rest = kv
            if key in out:
                raise YamlError(f"line {j + 1}: duplicate key {key!r}")
            self.i = j + 1
            rest = rest.strip()
            if rest[:1] in ("|", ">"):
                out[key] = self.block(ind, _strip_comment(rest))
            elif _strip_comment(rest) == "":
                k = self.meaningful()
                if k is not None and self._indent(self.lines[k]) == ind and self._is_item(self.lines[k].strip()):
                    out[key] = self.seq(ind)
                else:
                    out[key] = self.node(ind + 1)
            else:
                out[key] = _scalar(rest)

    def seq(self, ind):
        out = []
        while True:
            j = self.meaningful()
            if j is None:
                return out
            line = self.lines[j]
            text = line.strip()
            if self._indent(line) != ind or not self._is_item(text):
                if self._indent(line) > ind:
                    raise YamlError(f"line {j + 1}: unexpected indentation")
                return out
            content = text[1:].lstrip(" ")
            if not content or content.startswith("#"):
                self.i = j + 1
                out.append(self.node(ind + 1))
            elif _key_split(content) is not None:
                # `- key: value` starts a mapping at the column of `key`.
                col = ind + (len(text) - len(content))
                self.lines[j] = " " * col + content
                self.i = j
                out.append(self.mapping(col))
            else:
                self.i = j + 1
                if content[:1] in ("|", ">"):
                    raise YamlError(f"line {j + 1}: block scalar in a sequence")
                out.append(_scalar(content))

    def block(self, parent, header):
        style, chomp = header[0], header[1:]
        if chomp not in ("", "-", "+"):
            raise YamlError(f"unsupported block scalar header {header!r}")
        raw = []
        while self.i < len(self.lines):
            line = self.lines[self.i]
            if line.strip() and self._indent(line) <= parent:
                break
            raw.append(line)
            self.i += 1
        firsts = [l for l in raw if l.strip()]
        if not firsts:
            return ""
        ci = self._indent(firsts[0])
        body = [l[ci:] if l.strip() else "" for l in raw]
        trail = 0  # trailing blank lines belong to chomping
        while body and body[-1] == "":
            body.pop()
            trail += 1
        if style == "|":
            text = "\n".join(body)
        else:
            # Folding: a line break between two plain lines becomes a space;
            # blank lines and more-indented lines keep their breaks.
            text, prev = "", None
            for l in body:
                if prev is None:
                    text = l
                elif l == "":
                    text += "\n"
                elif prev == "" or l.startswith(" ") or prev.startswith(" "):
                    text += ("" if prev == "" else "\n") + l
                else:
                    text += " " + l
                prev = l
        if chomp == "-":
            return text
        if chomp == "+":
            return text + "\n" * (trail + 1)
        return text + "\n"


def load_yaml(path):
    with open(path) as f:
        src = f.read()
    loader = _Loader(src)
    doc = loader.node(0)
    if loader.meaningful() is not None:
        raise YamlError(f"{path}: trailing content at line {loader.meaningful() + 1}")
    return doc


def wf(name):
    return load_yaml(os.path.join(WF, name))


def step(workflow, job, name):
    for s in wf(workflow)["jobs"][job]["steps"]:
        if s.get("name") == name:
            return s
    raise KeyError(f"{workflow}:{job}:{name}")


def step_script(workflow, job, name):
    return step(workflow, job, name)["run"]


# ---- results ----------------------------------------------------------------
FAILS = []


def check(name, cond, detail=""):
    print(("PASS " if cond else "FAIL ") + name)
    if not cond:
        FAILS.append(name)
        print("     " + str(detail).replace("\n", "\n     "))


# ---- mock crates.io and GitHub API -------------------------------------------
class Mock:
    """Files under root are served; `routes` and `status` override paths."""

    def __init__(self):
        self.root = tempfile.mkdtemp()
        self.status = {}  # path -> forced status, empty body
        self.routes = {}  # path -> (status, body)
        self.fail_next = {}  # path -> how many more requests get a 503
        self.requests = []  # (path, headers)
        mock = self

        class H(http.server.BaseHTTPRequestHandler):
            def log_message(self, *a):
                pass

            def do_GET(self):
                path = self.path.split("?")[0]
                mock.requests.append((path, dict(self.headers)))
                if mock.fail_next.get(path, 0) > 0:
                    mock.fail_next[path] -= 1
                    self.send_response(503)
                    self.send_header("Content-Length", "0")
                    self.end_headers()
                    return
                if path in mock.status:
                    self.send_response(mock.status[path])
                    self.send_header("Content-Length", "0")
                    self.end_headers()
                    return
                if path in mock.routes:
                    code, body = mock.routes[path]
                    data = body.encode()
                else:
                    f = os.path.join(mock.root, path.lstrip("/"))
                    if os.path.isfile(f):
                        code, data = 200, open(f, "rb").read()
                    else:
                        code, data = 404, b""
                self.send_response(code)
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

        self.httpd = http.server.ThreadingHTTPServer(("127.0.0.1", 0), H)
        self.url = f"http://127.0.0.1:{self.httpd.server_address[1]}"
        threading.Thread(target=self.httpd.serve_forever, daemon=True).start()

    def agents(self):
        return [h.get("User-Agent") for _, h in self.requests]

    def index_path(self, crate):
        return os.path.join(self.root, "index", crate[:2], crate[2:4], crate)

    def add_index(self, crate, *versions):
        p = self.index_path(crate)
        os.makedirs(os.path.dirname(p), exist_ok=True)
        with open(p, "a") as f:
            for v in versions:
                f.write(json.dumps({"name": crate, "vers": v, "deps": [], "cksum": "x"},
                                   separators=(",", ":")) + "\n")

    def add_api(self, crate, version, yanked=False):
        p = os.path.join(self.root, "api", "crates", crate, version)
        os.makedirs(os.path.dirname(p), exist_ok=True)
        open(p, "w").write(json.dumps({"version": {"num": version, "yanked": yanked}}))

    def reset(self):
        shutil.rmtree(self.root)
        os.makedirs(self.root)
        self.status.clear()
        self.fail_next.clear()
        self.routes.clear()
        self.requests.clear()


MOCK = Mock()
WORK = tempfile.mkdtemp()
BIN = os.path.join(WORK, "bin")
os.makedirs(BIN)
CARGO_LOG = os.path.join(WORK, "cargo.log")
GH_LOG = os.path.join(WORK, "gh.log")

# Stub cargo: `metadata` prints ./metadata.json; anything else is logged, and
# `publish -p X` (not a dry-run) appends $VERSION to X's mock index file.
open(os.path.join(BIN, "cargo"), "w").write(f"""#!/bin/bash
if [ "$1" = metadata ]; then
  if [ -f metadata.json ]; then cat metadata.json; exit 0; fi
  echo "stub cargo: no metadata.json in $PWD" >&2; exit 101
fi
echo "$*" >> "{CARGO_LOG}"
if [ "$1" = publish ] && [ "$2" = -p ] && [ -n "${{STUB_INDEX_ROOT:-}}" ] && [ -z "${{STUB_NO_UPLOAD:-}}" ]; then
  c="$3"
  mkdir -p "$STUB_INDEX_ROOT/${{c:0:2}}/${{c:2:2}}"
  printf '{{"name":"%s","vers":"%s"}}\\n' "$c" "$VERSION" >> "$STUB_INDEX_ROOT/${{c:0:2}}/${{c:2:2}}/$c"
fi
exit "${{STUB_CARGO_EXIT:-0}}"
""")
# Stub gh: logs its arguments; `issue list` prints $STUB_GH_ISSUE (what the
# real gh prints after applying the step's --jq filter).
open(os.path.join(BIN, "gh"), "w").write(f"""#!/bin/bash
printf '%s\\n' "$*" >> "{GH_LOG}"
if [ "$1 $2" = "issue list" ]; then printf '%s' "${{STUB_GH_ISSUE:-}}"; fi
exit "${{STUB_GH_EXIT:-0}}"
""")
for tool in ("cargo", "gh"):
    os.chmod(os.path.join(BIN, tool), 0o755)

# Git without the user's or the system's configuration (no signing, hooks...).
GIT_ENV = {
    "GIT_CONFIG_GLOBAL": os.devnull,
    "GIT_CONFIG_NOSYSTEM": "1",
    "GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@example.invalid",
    "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@example.invalid",
}


def run(script, env=None, cwd=None):
    for f in (CARGO_LOG, GH_LOG):
        if os.path.exists(f):
            os.remove(f)
    out = os.path.join(WORK, "github_output")
    open(out, "w").close()
    runner_temp = tempfile.mkdtemp()
    path = os.path.join(WORK, "step.sh")
    open(path, "w").write(script)
    e = {
        "PATH": BIN + os.pathsep + os.environ["PATH"],
        "HOME": WORK,
        "GITHUB_OUTPUT": out,
        "RUNNER_TEMP": runner_temp,
        "CRATES_INDEX": MOCK.url + "/index",
        "CRATES_API": MOCK.url + "/api",
        "GITHUB_API_URL": MOCK.url + "/gh",
        "API_DELAY_SECONDS": "0",
        "STUB_INDEX_ROOT": os.path.join(MOCK.root, "index"),
    }
    e.update(GIT_ENV)
    e.update(env or {})
    # GitHub runs `run:` with `bash --noprofile --norc -eo pipefail {0}`.
    p = subprocess.run(["bash", "--noprofile", "--norc", "-eo", "pipefail", path],
                       env=e, cwd=cwd or WORK, capture_output=True, text=True)
    outputs = dict(l.split("=", 1) for l in open(out).read().splitlines() if "=" in l)
    cargo = open(CARGO_LOG).read().splitlines() if os.path.exists(CARGO_LOG) else []
    shutil.rmtree(runner_temp, ignore_errors=True)
    return p.returncode, p.stdout + p.stderr, outputs, cargo


def gh_calls():
    return open(GH_LOG).read().splitlines() if os.path.exists(GH_LOG) else []


# ---- git fixtures -------------------------------------------------------------
class Repo:
    """A clone with a bare `origin`, both in fresh temporary directories."""

    def __init__(self):
        self.origin = tempfile.mkdtemp()
        self.dir = tempfile.mkdtemp()
        self._git(["init", "-q", "--bare", self.origin], cwd=WORK)
        self.git("init", "-q")
        self.git("checkout", "-q", "-b", "main")
        self.git("remote", "add", "origin", self.origin)
        self.clock = int(time.time()) - 30 * 86400

    def _git(self, args, cwd=None, env=None):
        e = dict(os.environ, **GIT_ENV)
        e.update(env or {})
        p = subprocess.run(["git"] + args, cwd=cwd or self.dir, env=e,
                           capture_output=True, text=True)
        if p.returncode != 0:
            raise RuntimeError(f"git {' '.join(args)}: {p.stderr}")
        return p.stdout.strip()

    def git(self, *args, env=None):
        return self._git(list(args), env=env)

    def write(self, files):
        for rel, content in files.items():
            p = os.path.join(self.dir, rel)
            if content is None:
                if os.path.exists(p):
                    os.remove(p)
                continue
            os.makedirs(os.path.dirname(p), exist_ok=True)
            open(p, "w").write(content)

    def commit(self, msg, files=None, when=None):
        self.write(files or {})
        self.git("add", "-A")
        self.clock += 60
        t = f"{when if when is not None else self.clock} +0000"
        self.git("commit", "-q", "--allow-empty", "-m", msg,
                 env={"GIT_AUTHOR_DATE": t, "GIT_COMMITTER_DATE": t})
        return self.head()

    def head(self, ref="HEAD"):
        return self.git("rev-parse", ref + "^{commit}")

    def tag(self, name, ref="HEAD", when=None, annotated=True):
        t = f"{when if when is not None else self.clock} +0000"
        args = ["tag", "-a", name, "-m", name, ref] if annotated else ["tag", name, ref]
        self.git(*args, env={"GIT_COMMITTER_DATE": t})

    def push(self, *refs):
        self.git("push", "-q", "--force", "origin", *(refs or ("main",)))

    def origin_tag(self, name):
        try:
            return self._git(["rev-parse", f"refs/tags/{name}^{{commit}}"], cwd=self.origin)
        except RuntimeError:
            return None

    def origin_tags(self):
        return self._git(["tag", "-l"], cwd=self.origin).split()


CHANGELOG_HEAD = "# Changelog\n\n## [Unreleased]\n\n"


def release_files(version, ferroly=None, derive=None, req=None, changelog=None, script=True):
    """The files tag-on-merge and publish.yml look at, for <version>."""
    req = req if req is not None else f"^{version}"
    meta = {"packages": [
        {"name": "ferroly-derive", "version": derive or version, "dependencies": []},
        {"name": "ferroly", "version": ferroly or version,
         "dependencies": [{"name": "ferroly-derive", "req": req}]},
    ]}
    return {
        "Cargo.toml": (f'[workspace.package]\nversion = "{version}"\n\n'
                       f'[workspace.dependencies]\nferroly-derive = {{ path = "crates/ferroly-derive", '
                       f'version = "{req.lstrip("^=")}" }}\n'),
        "metadata.json": json.dumps(meta),
        "CHANGELOG.md": changelog if changelog is not None else
        CHANGELOG_HEAD + f"## [{version}] - 2026-11-01\n\nNotes.\n",
        ".github/scripts/check-release-version.sh": open(CHECK_SCRIPT).read() if script else None,
    }


# =============================================================================
# Workflow structure
# =============================================================================
PUBLISH = wf("publish.yml")
TAGGER = wf("tag-on-merge.yml")
DRIFT_WF = wf("crates-io-drift.yml")
ALERT_WF = wf("crates-io-drift-alert.yml")

# Publishing is manual for now: only a maintainer's dispatch starts publish.yml.
check("yaml: publish.yml has no `push: tags` trigger and no caller; dispatch only (REL-3)",
      sorted(PUBLISH[True]) == ["workflow_dispatch"], PUBLISH[True])
check("yaml: tag-on-merge only tags: one job, which calls no workflow",
      list(TAGGER["jobs"]) == ["tag"]
      and not any("uses" in j for j in TAGGER["jobs"].values()), list(TAGGER["jobs"]))
check("yaml: the drift check has no schedule while publishing is manual",
      "schedule" not in DRIFT_WF[True] and "workflow_dispatch" in DRIFT_WF[True], DRIFT_WF[True])
pub_steps = PUBLISH["jobs"]["publish"]["steps"]
check("yaml: the publish job checks out the commit verify checked, by SHA (REL-1)",
      pub_steps[0]["uses"].startswith("actions/checkout@")
      and pub_steps[0]["with"]["ref"] == "${{ needs.verify.outputs.sha }}", pub_steps[0])
names = [s.get("name", s.get("uses")) for s in pub_steps]
check("yaml: the publish job re-checks the tag before any credentials are touched (REL-1)",
      names.index("Check that the tag still points at the verified commit")
      < names.index("Authenticate with crates.io (trusted publishing)"), names)
check("yaml: verify outputs the SHA it checked",
      PUBLISH["jobs"]["verify"]["outputs"]["sha"] == "${{ steps.check.outputs.sha }}")
for wfname, doc, job in (("publish.yml", PUBLISH, "verify"), ("tag-on-merge.yml", TAGGER, "tag"),
                         ("crates-io-drift.yml", DRIFT_WF, "drift"),
                         ("crates-io-drift-alert.yml", ALERT_WF, "alert")):
    check(f"yaml: {wfname} {job} runs only in nandlabs/ferroly, not in forks (REL-10)",
          "github.repository == 'nandlabs/ferroly'" in str(doc["jobs"][job].get("if", "")),
          doc["jobs"][job].get("if"))
check("yaml: the post-publish drift check is for the tag just published (REL-7)",
      PUBLISH["jobs"]["drift"]["with"]["tag"] == "${{ inputs.tag }}", PUBLISH["jobs"]["drift"])
check("yaml: the drift workflow needs only contents: read (publish.yml calls it)",
      DRIFT_WF["permissions"] == {"contents": "read"}
      and all("permissions" not in j for j in DRIFT_WF["jobs"].values()))
check("yaml: the drift alert runs only for failed scheduled runs (REL-8)",
      "conclusion == 'failure'" in ALERT_WF["jobs"]["alert"]["if"]
      and "event == 'schedule'" in ALERT_WF["jobs"]["alert"]["if"]
      and ALERT_WF["jobs"]["alert"]["permissions"] == {"issues": "write"}
      and not any("uses" in s for s in ALERT_WF["jobs"]["alert"]["steps"]))
check("yaml: the protected-environment check runs in verify, before the publish job (REL-2)",
      any(s.get("name") == "Check that the crates-io environment is protected"
          for s in PUBLISH["jobs"]["verify"]["steps"]))

ACTION_PIN = re.compile(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_./-]+@[0-9a-f]{40}$")
bad_pins, injections = [], []
for path in sorted(glob.glob(os.path.join(WF, "*.yml"))):
    doc = load_yaml(path)
    for jname, job in doc["jobs"].items():
        if "uses" in job and not job["uses"].startswith("./"):
            bad_pins.append(f"{os.path.basename(path)}:{jname}")
        for s in job.get("steps", []):
            if "uses" in s and not ACTION_PIN.match(s["uses"]):
                bad_pins.append(f"{os.path.basename(path)}:{jname}: {s['uses']}")
            if "${{" in str(s.get("run", "")):
                injections.append(f"{os.path.basename(path)}:{jname}:{s.get('name')}")
check("yaml: every action is pinned to a full commit SHA", not bad_pins, bad_pins)
check("yaml: no `${{ }}` expression inside any run: script (values go through env) (SVC-16)",
      not injections, injections)

try:
    import yaml as pyyaml  # optional: cross-check the subset loader
except ImportError:
    pyyaml = None
if pyyaml is not None:
    diffs = [os.path.basename(p) for p in sorted(glob.glob(os.path.join(WF, "*.yml")))
             if pyyaml.safe_load(open(p)) != load_yaml(p)]
    check("yaml: the subset loader agrees with PyYAML on every workflow", not diffs, diffs)
else:
    print("SKIP yaml: PyYAML is not installed; subset loader not cross-checked")


def helpers(script):
    m = re.search(r"# --- index helpers.*?\n(.*?)# --- end index helpers ---", script, re.S)
    return m.group(1) if m else None


DRY = step_script("publish.yml", "verify", "cargo publish --dry-run")
PUB = step_script("publish.yml", "publish", "Publish ferroly-derive, then ferroly")
check("publish: verify and publish use the same three-state index lookup (REL-9, SVC-8)",
      helpers(DRY) is not None and helpers(DRY) == helpers(PUB))

# =============================================================================
# check-release-version.sh
# =============================================================================


def check_script(version, **kw):
    r = Repo()
    r.commit("release", release_files("0.6.0", **kw))
    return run(f'exec bash "{CHECK_SCRIPT}" "$V"', {"V": version}, cwd=r.dir)


rc, log, _, _ = check_script("0.6.0")
check("check-version: matching versions, requirement and changelog -> pass",
      rc == 0 and "OK: ferroly and ferroly-derive are 0.6.0" in log, log)
rc, log, _, _ = check_script("0.6.0", req="=0.6.0")
check("check-version: an exact `=` requirement is accepted", rc == 0, log)
rc, log, _, _ = check_script("0.6.0", ferroly="0.5.9")
check("check-version: ferroly at another version -> fail",
      rc == 1 and "ferroly is 0.5.9, expected 0.6.0" in log, log)
rc, log, _, _ = check_script("0.6.0", derive="0.5.9")
check("check-version: ferroly-derive at another version -> fail",
      rc == 1 and "ferroly-derive is 0.5.9, expected 0.6.0" in log, log)
rc, log, _, _ = check_script("0.6.0", req="^0.5.0")
check("check-version: ferroly requires an older ferroly-derive -> fail (SVC-7)",
      rc == 1 and "requires ferroly-derive ['^0.5.0']" in log, log)
rc, log, _, _ = check_script("0.6.0", changelog=CHANGELOG_HEAD + "## [0.5.0]\n")
check("check-version: no changelog heading for the version -> fail",
      rc == 1 and "CHANGELOG.md has no '## [0.6.0]' entry" in log, log)
rc, log, _, _ = check_script("0.6.0", changelog=CHANGELOG_HEAD + "## [0.6.0x]\n## [0a6b0]\n")
check("check-version: changelog heading match is exact", rc == 1, log)
rc, log, _, _ = check_script("0.6.0", changelog=CHANGELOG_HEAD + "## [0.6.0]\n")
check("check-version: a heading without a date counts", rc == 0, log)
for bad in ("0.6", "v0.6.0", "0.6.0\n0.6.0", "0.6.0 ", "0.6.0;true", ""):
    rc, log, _, _ = check_script(bad)
    check(f"check-version: {bad!r} is not a version -> fail", rc == 1 and "is not a" in log, log)

# =============================================================================
# publish.yml verify: "Check the tag, the versions and the changelog"
# =============================================================================
VERIFY = step_script("publish.yml", "verify", "Check the tag, the versions and the changelog")


def verify_repo():
    """main: 0.4.0 (tag v0.4.0) and 0.5.0 (tag v0.5.0), both from before the
    shared check script existed, then the commit that adds it (GITHUB_SHA)."""
    r = Repo()
    r.commit("release 0.4.0", release_files("0.4.0", script=False))
    r.tag("v0.4.0")
    r.commit("release 0.5.0", release_files("0.5.0", script=False))
    r.tag("v0.5.0")
    r.workflow_sha = r.commit("ci: add the check script", release_files("0.5.0"))
    r.push("main", "--tags")
    return r


def verify(r, tag, **env):
    r.git("checkout", "-q", "--detach", f"refs/tags/{tag}")
    e = {"RELEASE_TAG": tag, "GITHUB_REF": "refs/heads/main", "GITHUB_SHA": r.workflow_sha}
    e.update(env)
    return run(VERIFY, e, cwd=r.dir)


R = verify_repo()
rc, log, out, _ = verify(R, "v0.5.0")
check("verify: a tag on main with matching versions and changelog -> pass, outputs version and SHA",
      rc == 0 and out.get("version") == "0.5.0" and out.get("sha") == R.head("v0.5.0"), f"{rc} {out}\n{log}")
rc, log, out, _ = verify(R, "v0.4.0")
check("verify: an older tag whose tree predates the check script still verifies (backlog)",
      rc == 0 and out.get("version") == "0.4.0", f"{rc}\n{log}")
rc, log, out, _ = verify(R, "v0.5.0", GITHUB_REF="refs/heads/feature")
check("verify: run from a branch other than main -> fail (REL-3)",
      rc == 1 and "Run this workflow from main" in log and "sha" not in out, log)
rc, log, out, _ = verify(R, "v0.5.0", GITHUB_REF="refs/tags/v0.5.0")
check("verify: run for a tag ref (the old push trigger) -> fail (REL-3)", rc == 1, log)

for bad in ("v0.5", "0.5.0", "v0.5.0;id", "v0.5.0\nv0.5.0", "v0.5.0 "):
    rc, log, out, _ = run(VERIFY, {"RELEASE_TAG": bad, "GITHUB_REF": "refs/heads/main",
                                   "GITHUB_SHA": R.workflow_sha}, cwd=R.dir)
    check(f"verify: tag {bad!r} is not v<semver> -> fail", rc == 1 and "is not a v<semver> tag" in log, log)


def verify_with(tag_version, **kw):
    r = verify_repo()
    r.git("checkout", "-q", "main")
    r.commit("release", release_files(tag_version, **kw))
    r.tag(f"v{tag_version}")
    r.workflow_sha = r.commit("later", dict(release_files(tag_version, **kw), **{"docs/x.md": "x"}))
    r.push("main", "--tags")
    return verify(r, f"v{tag_version}")


rc, log, _, _ = verify_with("0.6.0", ferroly="0.5.0")
check("verify: ferroly's version differs from the tag -> fail", rc == 1 and "ferroly is 0.5.0" in log, log)
rc, log, _, _ = verify_with("0.6.0", derive="0.5.0")
check("verify: ferroly-derive's version differs from the tag -> fail",
      rc == 1 and "ferroly-derive is 0.5.0" in log, log)
rc, log, _, _ = verify_with("0.6.0", req="^0.5.0")
check("verify: wrong ferroly-derive requirement -> fail", rc == 1 and "requires ferroly-derive" in log, log)
rc, log, _, _ = verify_with("0.6.0", changelog=CHANGELOG_HEAD)
check("verify: missing changelog heading -> fail", rc == 1 and "has no '## [0.6.0]' entry" in log, log)

# A tag on a commit that never reached main.
R = verify_repo()
R.git("checkout", "-q", "-b", "side", "v0.5.0")
R.commit("side release", release_files("0.6.0"))
R.tag("v0.6.0")
R.push("side", "--tags")
rc, log, out, _ = verify(R, "v0.6.0")
check("verify: tag off main -> fail", rc == 1 and "is not on main" in log and "sha" not in out, log)

# The tag's own copy of the check script is ignored: the workflow commit's runs.
R = verify_repo()
R.git("checkout", "-q", "main")
files = release_files("0.6.0", ferroly="0.5.0")
files[".github/scripts/check-release-version.sh"] = "#!/bin/bash\nexit 0\n"
R.commit("release with a gutted check", files)
R.tag("v0.6.0")
R.push("main", "--tags")
rc, log, _, _ = verify(R, "v0.6.0")
check("verify: runs the check script from the workflow's commit, not the tagged tree",
      rc == 1 and "ferroly is 0.5.0" in log, log)

# =============================================================================
# publish.yml verify: "Check that the crates-io environment is protected"
# =============================================================================
ENVCHECK = step_script("publish.yml", "verify", "Check that the crates-io environment is protected")
ENV_PATH = "/gh/repos/nandlabs/ferroly/environments/crates-io"
GH = {"GH_TOKEN": "gh-token", "GITHUB_REPOSITORY": "nandlabs/ferroly"}


def environment(reviewers=True, policy="custom", rules=(("main", "branch"),)):
    prot = []
    if reviewers:
        prot.append({"id": 1, "type": "required_reviewers", "prevent_self_review": True,
                     "reviewers": [{"type": "User", "reviewer": {"login": "m"}}]})
    prot.append({"id": 2, "type": "branch_policy"})
    dbp = None if policy is None else {"protected_branches": policy == "protected",
                                       "custom_branch_policies": policy == "custom"}
    MOCK.reset()
    MOCK.routes[ENV_PATH] = (200, json.dumps({"name": "crates-io", "protection_rules": prot,
                                              "deployment_branch_policy": dbp}))
    MOCK.routes[ENV_PATH + "/deployment-branch-policies"] = (200, json.dumps(
        {"total_count": len(rules),
         "branch_policies": [{"id": i, "name": n, "type": t} for i, (n, t) in enumerate(rules)]}))


environment()
rc, log, _, _ = run(ENVCHECK, GH)
check("environment: reviewers and main-only deployments -> pass",
      rc == 0 and "OK: crates-io requires a reviewer" in log, log)
check("environment: asks the API with the job's token",
      MOCK.requests and all(h.get("Authorization") == "Bearer gh-token" for _, h in MOCK.requests),
      MOCK.requests)
MOCK.reset()
rc, log, _, _ = run(ENVCHECK, GH)
check("environment: does not exist (GitHub would create it unprotected) -> fail (REL-2)",
      rc == 1 and "does not exist" in log, log)
environment(reviewers=False)
rc, log, _, _ = run(ENVCHECK, GH)
check("environment: no required reviewers -> fail (REL-2)", rc == 1 and "no required reviewers" in log, log)
environment(policy=None)
rc, log, _, _ = run(ENVCHECK, GH)
check("environment: deploys from any branch -> fail", rc == 1 and "deploys from any branch" in log, log)
environment(policy="protected")
rc, log, _, _ = run(ENVCHECK, GH)
check("environment: 'protected branches' instead of main only -> fail", rc == 1, log)
environment(rules=(("main", "branch"), ("v*", "tag")))
rc, log, _, _ = run(ENVCHECK, GH)
check("environment: a `v*` tag deployment rule -> fail (REL-3)", rc == 1 and "no tag rules" in log, log)
environment(rules=(("*", "branch"),))
rc, log, _, _ = run(ENVCHECK, GH)
check("environment: a wildcard branch rule -> fail", rc == 1, log)
environment()
MOCK.status[ENV_PATH] = 500
rc, log, _, _ = run(ENVCHECK, GH)
check("environment: API error -> fail", rc == 1 and "could not read" in log, log)

# =============================================================================
# publish.yml verify: "cargo publish --dry-run"
# =============================================================================
V = {"VERSION": "0.5.0"}

MOCK.reset()
MOCK.add_index("ferroly", "0.3.2")
MOCK.add_index("ferroly-derive", "0.3.2")
rc, log, out, cargo = run(DRY, V)
check("dry-run: neither published -> one multi-package dry-run of both",
      rc == 0 and cargo == ["publish --dry-run --locked -p ferroly-derive -p ferroly"]
      and out.get("crates") == "ferroly-derive ferroly", f"{rc} {cargo} {out}\n{log}")

MOCK.reset()
MOCK.add_index("ferroly", "0.3.2")  # ferroly-derive not in the index at all (404)
rc, log, out, cargo = run(DRY, V)
check("dry-run: crate missing from the index (404) counts as unpublished",
      rc == 0 and out.get("crates") == "ferroly-derive ferroly", f"{rc} {cargo} {out}\n{log}")

MOCK.reset()
MOCK.add_index("ferroly-derive", "0.3.2", "0.5.0")
MOCK.add_index("ferroly", "0.3.2")
rc, log, out, cargo = run(DRY, V)
check("dry-run: derive already published -> dry-run ferroly only",
      rc == 0 and cargo == ["publish --dry-run --locked -p ferroly"] and out.get("crates") == "ferroly",
      f"{rc} {cargo} {out}\n{log}")

MOCK.reset()
MOCK.add_index("ferroly-derive", "0.5.0")
MOCK.add_index("ferroly", "0.5.0")
rc, log, out, cargo = run(DRY, V)
check("dry-run: both published -> idempotent skip, no cargo call, empty pending list",
      rc == 0 and cargo == [] and out.get("crates") == "", f"{rc} {cargo} {out}\n{log}")

MOCK.reset()
MOCK.add_index("ferroly-derive", "0a5b0", "10.5.0", "0.5.01")
MOCK.add_index("ferroly", "0.3.2")
rc, log, out, cargo = run(DRY, V)
check("dry-run: version match is exact (no regex dots, no prefix/suffix match)",
      rc == 0 and out.get("crates") == "ferroly-derive ferroly", f"{rc} {out}\n{log}")

MOCK.reset()
MOCK.status["/index/fe/rr/ferroly-derive"] = 503
rc, log, out, cargo = run(DRY, V)
check("dry-run: unreadable index (HTTP 503) fails instead of guessing",
      rc != 0 and cargo == [] and "could not read the index" in log, f"{rc} {cargo}\n{log}")

MOCK.reset()
MOCK.add_index("ferroly", "0.3.2")
rc, log, out, cargo = run(DRY, dict(V, STUB_CARGO_EXIT="101"))
check("dry-run: a failing cargo dry-run fails the job", rc == 101, f"{rc}\n{log}")

# =============================================================================
# publish.yml publish: "Check that the tag still points at the verified commit"
# =============================================================================
TAGCHECK = step_script("publish.yml", "publish", "Check that the tag still points at the verified commit")
R = Repo()
c1 = R.commit("release", release_files("0.5.0"))
R.tag("v0.5.0")
c2 = R.commit("other", {"crates/x.rs": "y"})
R.tag("v0.5.1-light", annotated=False, ref=c1)
R.push("main", "--tags")
rc, log, _, _ = run(TAGCHECK, {"RELEASE_TAG": "v0.5.0", "VERIFIED_SHA": c1}, cwd=R.dir)
check("tag-check: annotated tag still at the verified commit -> pass", rc == 0, log)
rc, log, _, _ = run(TAGCHECK, {"RELEASE_TAG": "v0.5.1-light", "VERIFIED_SHA": c1}, cwd=R.dir)
check("tag-check: lightweight tag still at the verified commit -> pass", rc == 0, log)
R.git("tag", "-f", "-a", "v0.5.0", "-m", "moved", c2)
R.push("--tags")
rc, log, _, _ = run(TAGCHECK, {"RELEASE_TAG": "v0.5.0", "VERIFIED_SHA": c1}, cwd=R.dir)
check("tag-check: tag force-moved after verify -> fail, nothing uploaded (REL-1)",
      rc == 1 and f"is now {c2}" in log, log)
R._git(["tag", "-d", "v0.5.0"], cwd=R.origin)
rc, log, _, _ = run(TAGCHECK, {"RELEASE_TAG": "v0.5.0", "VERIFIED_SHA": c1}, cwd=R.dir)
check("tag-check: tag deleted after verify -> fail", rc == 1 and "is now gone" in log, log)

# =============================================================================
# publish.yml publish: "Publish ferroly-derive, then ferroly"
# =============================================================================
FAST = dict(V, CARGO_REGISTRY_TOKEN="t", INDEX_POLLS="3", INDEX_POLL_SECONDS="0")
UP = "publish -p {} --locked --no-verify"

MOCK.reset()
MOCK.add_index("ferroly-derive", "0.3.2")
MOCK.add_index("ferroly", "0.3.2")
rc, log, out, cargo = run(PUB, FAST)
check("publish: derive first, waits for the index, then ferroly; uploads with --no-verify (REL-4)",
      rc == 0 and cargo == [UP.format("ferroly-derive"), UP.format("ferroly")]
      and log.index("ferroly-derive 0.5.0 is in the index") < log.index("ferroly 0.5.0 is in the index"),
      f"{rc} {cargo}\n{log}")

MOCK.reset()
MOCK.add_index("ferroly-derive", "0.5.0")
MOCK.add_index("ferroly", "0.3.2")
rc, log, out, cargo = run(PUB, FAST)
check("publish: re-run after a partial publish skips derive, publishes ferroly",
      rc == 0 and cargo == [UP.format("ferroly")] and "ferroly-derive 0.5.0 is already published" in log,
      f"{rc} {cargo}\n{log}")

MOCK.reset()
MOCK.add_index("ferroly-derive", "0.5.0")
MOCK.add_index("ferroly", "0.5.0")
rc, log, out, cargo = run(PUB, FAST)
check("publish: everything already published -> no upload, success", rc == 0 and cargo == [],
      f"{rc} {cargo}\n{log}")

# A big index file with the version on its first line: `curl | grep -q` under
# pipefail reports "not published" when grep exits before curl finishes.
MOCK.reset()
p = MOCK.index_path("ferroly-derive")
os.makedirs(os.path.dirname(p), exist_ok=True)
with open(p, "w") as f:
    f.write('{"name":"ferroly-derive","vers":"0.5.0"}\n')
    f.write(('{"name":"ferroly-derive","vers":"0.0.1","pad":"' + "x" * 4000 + '"}\n') * 1000)
MOCK.add_index("ferroly", "0.5.0")
rc, log, out, cargo = run(PUB, FAST)
check("publish: version on line 1 of a large index file is found (SVC-8)",
      rc == 0 and cargo == [], f"{rc} {cargo}\n{log}")

MOCK.reset()
MOCK.add_index("ferroly-derive", "0.3.2")
rc, log, out, cargo = run(PUB, dict(FAST, STUB_NO_UPLOAD="1"))
check("publish: index never serves the version -> times out, ferroly not published",
      rc != 0 and cargo == [UP.format("ferroly-derive")] and "did not appear" in log
      and "within 0 seconds" in log, f"{rc} {cargo}\n{log}")

MOCK.reset()
MOCK.status["/index/fe/rr/ferroly-derive"] = 503
rc, log, out, cargo = run(PUB, FAST)
check("publish: unreadable index before an upload -> hard error, nothing uploaded (REL-9)",
      rc == 1 and cargo == [] and "could not read the crates.io index for ferroly-derive" in log,
      f"{rc} {cargo}\n{log}")

# The index answers 503 to the first poll after the upload (curl's own
# retries included), then serves the version: the error is retried like a miss.
MOCK.reset()
MOCK.add_index("ferroly-derive", "0.3.2")
MOCK.add_index("ferroly", "0.5.0")
script = PUB.replace('cargo publish -p "$crate" --locked --no-verify',
                     'cargo publish -p "$crate" --locked --no-verify; '
                     'curl -s "$FLAKY_URL" >/dev/null || true')
assert script != PUB
# The stub "uploads" first; the trigger request then arms four 503s.
ARM = "/arm-flaky"
orig_do_get = MOCK.httpd.RequestHandlerClass.do_GET


def do_get(handler):
    if handler.path == ARM:
        MOCK.fail_next["/index/fe/rr/ferroly-derive"] = 4  # curl tries 1 + 3 times
        handler.send_response(204)
        handler.end_headers()
        return
    orig_do_get(handler)


MOCK.httpd.RequestHandlerClass.do_GET = do_get
rc, log, out, cargo = run(script, dict(FAST, INDEX_POLLS="3", FLAKY_URL=MOCK.url + ARM))
MOCK.httpd.RequestHandlerClass.do_GET = orig_do_get
check("publish: an index error while waiting is retried like a miss (REL-9)",
      rc == 0 and cargo == [UP.format("ferroly-derive")] and "ferroly-derive 0.5.0 is in the index" in log
      and "could not read the index for ferroly-derive" in log, f"{rc} {cargo}\n{log}")

MOCK.reset()
rc, log, out, cargo = run(PUB, dict(V, CARGO_REGISTRY_TOKEN=""))
check("publish: no credentials -> fails before any upload",
      rc != 0 and cargo == [] and "No crates.io credentials" in log, f"{rc} {cargo}\n{log}")

MOCK.reset()
rc, log, out, cargo = run(PUB, dict(FAST, STUB_CARGO_EXIT="101"))
check("publish: a failing cargo publish stops the job", rc == 101 and len(cargo) == 1, f"{rc} {cargo}\n{log}")

# =============================================================================
# tag-on-merge.yml: which commit gets the tag (SVC-1)
# =============================================================================
TAGSTEP = step_script("tag-on-merge.yml", "tag", "Tag the crate version if untagged")
ZERO = "0" * 40


def base_repo():
    r = Repo()
    r.commit("release 0.5.0", dict(release_files("0.5.0"), **{"crates/lib.rs": "v5"}))
    r.tag("v0.5.0")
    r.push("main", "--tags")
    return r


def tag_run(r, before):
    r.git("checkout", "-q", "main")
    r.push("main")
    rc, log, out, _ = run(TAGSTEP, {"BEFORE": before}, cwd=r.dir)
    return rc, log, out


bump = release_files

# Squash merge: one commit.
R = base_repo()
before = R.head()
S = R.commit("feat: 0.6.0 (#1)", dict(bump("0.6.0"), **{"crates/lib.rs": "v6"}))
rc, log, out = tag_run(R, before)
check("tag: squash merge -> the squash commit",
      rc == 0 and out.get("created") == "true" and out.get("tag") == "v0.6.0"
      and R.origin_tag("v0.6.0") == S, f"{rc} {out}\n{log}")

# Rebase merge / fast-forward: the branch's commits land on main one by one,
# the version bump first.
R = base_repo()
before = R.head()
b1 = R.commit("chore(release): start 0.6.0", bump("0.6.0"))
R.commit("fix 1", {"crates/lib.rs": "fix 1"})
b3 = R.commit("fix 2", {"crates/lib.rs": "fix 2"})
rc, log, out = tag_run(R, before)
check("tag: rebase merge / fast-forward -> the last commit, not the one that bumped the version",
      rc == 0 and R.origin_tag("v0.6.0") == b3, f"{rc} tagged {R.origin_tag('v0.6.0')} b1={b1} b3={b3}\n{log}")

# Merge commit, with main having moved on meanwhile.
R = base_repo()
c0 = R.head()
d = R.commit("docs on main", {"docs/a.md": "a"})
R.git("checkout", "-q", "-b", "feat", c0)
R.commit("chore(release): start 0.6.0", bump("0.6.0"))
R.commit("fix", {"crates/lib.rs": "fix"})
R.git("checkout", "-q", "main")
R.git("merge", "-q", "--no-ff", "-m", "Merge #3", "feat")
M = R.head()
rc, log, out = tag_run(R, d)
check("tag: merge commit -> the merge commit", rc == 0 and R.origin_tag("v0.6.0") == M,
      f"{rc} tagged {R.origin_tag('v0.6.0')} M={M}\n{log}")

# Recovery: the bumping push's run failed; a later docs-only push finds the
# release commit (rebase, squash and merge-commit histories).
R = base_repo()
R.commit("chore(release): start 0.6.0", bump("0.6.0"))
R.commit("fix 1", {"crates/lib.rs": "fix 1"})
b3 = R.commit("fix 2", {"crates/lib.rs": "fix 2"})
before = R.commit("docs", {"docs/a.md": "a"})
R.commit("ci", {".github/workflows/x.yml": "x"})
rc, log, out = tag_run(R, before)
check("tag: recovery after a rebase merge -> its last commit, not the later docs pushes",
      rc == 0 and R.origin_tag("v0.6.0") == b3 and "Recovering" in log, f"{rc} {out}\n{log}")

R = base_repo()
S = R.commit("feat: 0.6.0 (#1)", dict(bump("0.6.0"), **{"crates/lib.rs": "v6"}))
before = R.head()
R.commit("docs", {"docs/a.md": "a"})
rc, log, out = tag_run(R, before)
check("tag: recovery after a squash merge -> the squash commit", rc == 0 and R.origin_tag("v0.6.0") == S,
      f"{rc}\n{log}")

R = base_repo()
c0 = R.head()
R.git("checkout", "-q", "-b", "feat", c0)
R.commit("chore(release): start 0.6.0", bump("0.6.0"))
R.commit("fix", {"crates/lib.rs": "fix"})
R.git("checkout", "-q", "main")
R.git("merge", "-q", "--no-ff", "-m", "Merge #3", "feat")
M = R.head()
R.commit("docs", {"docs/a.md": "a"})
rc, log, out = tag_run(R, M)
check("tag: recovery after a merge commit -> the merge commit", rc == 0 and R.origin_tag("v0.6.0") == M,
      f"{rc}\n{log}")

# The changelog entry came in the branch's last commit: that commit is the
# release, although the version was bumped earlier.
R = base_repo()
R.commit("bump", dict(bump("0.6.0"), **{"CHANGELOG.md": CHANGELOG_HEAD + "## [0.5.0]\n"}))
last = R.commit("changelog", {"CHANGELOG.md": CHANGELOG_HEAD + "## [0.6.0]\n\n## [0.5.0]\n"})
before = R.head()
R.commit("docs", {"docs/a.md": "a"})
rc, log, out = tag_run(R, before)
check("tag: recovery picks the first commit with the release's full content (changelog included)",
      rc == 0 and R.origin_tag("v0.6.0") == last, f"{rc}\n{log}")

R = base_repo()
S = R.commit("feat: 0.6.0 (#1)", dict(bump("0.6.0"), **{"crates/lib.rs": "v6"}))
rc, log, out = tag_run(R, ZERO)
check("tag: no previous commit (all-zero `before`) -> recovery, the release commit",
      rc == 0 and R.origin_tag("v0.6.0") == S, f"{rc}\n{log}")
R = base_repo()
S = R.commit("feat: 0.6.0 (#1)", dict(bump("0.6.0"), **{"crates/lib.rs": "v6"}))
rc, log, out = tag_run(R, "1234567890123456789012345678901234567890")
check("tag: `before` no longer exists (force push) -> recovery", rc == 0 and R.origin_tag("v0.6.0") == S,
      f"{rc}\n{log}")

R = base_repo()
before = R.head()
R.commit("docs", {"docs/a.md": "a"})
rc, log, out = tag_run(R, before)
check("tag: version already tagged -> nothing to do",
      rc == 0 and out.get("created") == "false" and R.origin_tags() == ["v0.5.0"], f"{rc} {out}\n{log}")

R = base_repo()
before = R.head()
R.commit("feat: 0.6.0", release_files("0.6.0", req="^0.5.0"))
rc, log, out = tag_run(R, before)
check("tag: derive requirement not bumped -> no tag is created (SVC-7)",
      rc == 1 and R.origin_tags() == ["v0.5.0"] and "requires ferroly-derive" in log, f"{rc}\n{log}")
R = base_repo()
before = R.head()
R.commit("feat: 0.6.0", release_files("0.6.0", changelog=CHANGELOG_HEAD))
rc, log, out = tag_run(R, before)
check("tag: no changelog entry -> no tag is created (SVC-7)",
      rc == 1 and R.origin_tags() == ["v0.5.0"], f"{rc}\n{log}")

# =============================================================================
# crates-io-drift.yml
# =============================================================================
DRIFT = step_script("crates-io-drift.yml", "drift", "Check the release tags against crates.io")
OLD = 3 * 86400


def drift_repo(tags, tag_age_seconds=OLD, version=None, version_age=None):
    r = Repo()
    now = int(time.time())
    when = now - (version_age if version_age is not None else OLD)
    r.commit("c", {"Cargo.toml": f'[workspace.package]\nversion = "{version or "0.0.0"}"\n'}, when=when)
    for t in tags:
        r.tag(t, when=now - tag_age_seconds)
    return r.dir


def drift(cwd, **env):
    MOCK.requests.clear()
    e = {"GRACE_HOURS": "0"}
    e.update(env)
    return run(DRIFT, e, cwd=cwd)


TAGS = ["v0.3.2", "v0.9.0", "v0.10.0-rc.1", "v0.10.0", "v0.2.0", "not-a-tag", "vfoo"]
repo = drift_repo(TAGS, version="0.10.0")

MOCK.reset()
for c in ("ferroly-derive", "ferroly"):
    MOCK.add_api(c, "0.10.0")
rc, log, out, _ = drift(repo)
check("drift: newest tag picked by version (v0.10.0 > v0.9.0 > v0.10.0-rc.1), both present -> pass, stop there",
      rc == 0 and "OK: v0.10.0 is on crates.io" in log and len(MOCK.requests) == 2
      and "OK: 0.10.0, the version on this branch, is tagged" in log, f"{rc} {MOCK.requests}\n{log}")
check("drift: sends the generic ferroly-ci User-Agent",
      MOCK.agents() and all(a == "ferroly-ci (+https://github.com/nandlabs/ferroly)" for a in MOCK.agents()),
      str(MOCK.agents()))

MOCK.reset()
MOCK.add_api("ferroly-derive", "0.10.0")
for c in ("ferroly-derive", "ferroly"):
    MOCK.add_api(c, "0.10.0-rc.1")
rc, log, out, _ = drift(repo)
check("drift: ferroly missing for the newest tag -> fail naming it; older published tag ends the scan",
      rc == 1 and "v0.10.0 is not on crates.io (missing: ferroly)" in log
      and "OK: v0.10.0-rc.1 is on crates.io" in log and "v0.9.0" not in log, f"{rc}\n{log}")

# Today's backlog: 0.5.0 and 0.4.0 both unpublished, 0.3.2 on crates.io.
backlog = drift_repo(["v0.3.2", "v0.4.0", "v0.5.0"], version="0.5.0")
MOCK.reset()
for c in ("ferroly-derive", "ferroly"):
    MOCK.add_api(c, "0.3.2")
rc, log, out, _ = drift(backlog)
check("drift: every unpublished tag above the newest published one is reported (REL-7)",
      rc == 1 and "v0.5.0 is not on crates.io (missing: ferroly-derive ferroly)" in log
      and "v0.4.0 is not on crates.io (missing: ferroly-derive ferroly)" in log
      and "OK: v0.3.2 is on crates.io" in log, f"{rc}\n{log}")

MOCK.reset()
for c in ("ferroly-derive", "ferroly"):
    MOCK.add_api(c, "0.4.0")
rc, log, out, _ = drift(backlog, TAG="v0.4.0")
check("drift: with a tag, only that tag is checked (re-publishing an old tag goes green) (REL-7)",
      rc == 0 and "OK: v0.4.0 is on crates.io" in log and "v0.5.0" not in log
      and "version on this branch" not in log, f"{rc}\n{log}")
rc, log, out, _ = drift(backlog, TAG="v0.5.0")
check("drift: with a tag that is not on crates.io -> fail", rc == 1 and "v0.5.0 is not on crates.io" in log, log)
rc, log, out, _ = drift(backlog, TAG="v9.9.9")
check("drift: with a tag that does not exist -> fail", rc == 1 and "there is no tag v9.9.9" in log, log)
rc, log, out, _ = drift(backlog, TAG="v0.4.0\nv0.5.0")
check("drift: with a malformed tag -> fail", rc == 1 and "is not a v<semver> tag" in log, log)

# The version on main was never tagged (tag-on-merge failed).
untagged = drift_repo(["v0.5.0"], version="0.6.0")
MOCK.reset()
for c in ("ferroly-derive", "ferroly"):
    MOCK.add_api(c, "0.5.0")
rc, log, out, _ = drift(untagged)
check("drift: the version on main has no tag -> fail (REL-7)",
      rc == 1 and "0.6.0, the version on this branch, has no tag v0.6.0" in log, f"{rc}\n{log}")
fresh_untagged = drift_repo(["v0.5.0"], version="0.6.0", version_age=600)
rc, log, out, _ = drift(fresh_untagged, GRACE_HOURS="24")
check("drift: an untagged version younger than the grace period -> warning, pass",
      rc == 0 and "::warning::0.6.0 has no tag v0.6.0 yet" in log, f"{rc}\n{log}")

MOCK.reset()
rc, log, out, _ = drift(repo, GRACE_HOURS="24")
check("drift: tag older than the grace period still fails", rc == 1, f"{rc}\n{log}")

fresh = drift_repo(["v0.6.0"], 600, version="0.6.0")
MOCK.reset()
rc, log, out, _ = drift(fresh, GRACE_HOURS="24")
check("drift: fresh tag within the grace period -> warning, pass",
      rc == 0 and "::warning::v0.6.0 is not on crates.io yet" in log, f"{rc}\n{log}")
rc, log, out, _ = drift(fresh)
check("drift: grace 0 (after publish / by hand) -> fresh missing tag fails", rc == 1, f"{rc}\n{log}")
rc, log, out, _ = run(DRIFT, {}, cwd=fresh)
check("drift: unset grace defaults to 0", rc == 1 and "whole number" not in log, f"{rc}\n{log}")
for bad in ("x", "1\n2", "-1"):
    rc, log, out, _ = drift(fresh, GRACE_HOURS=bad)
    check(f"drift: invalid grace value {bad!r} is rejected", rc == 1 and "whole number" in log, log)

MOCK.reset()
MOCK.add_api("ferroly-derive", "0.10.0", yanked=True)
MOCK.add_api("ferroly", "0.10.0")
rc, log, out, _ = drift(repo)
check("drift: yanked counts as present, with a warning",
      rc == 0 and "::warning::ferroly-derive 0.10.0 is on crates.io but yanked" in log, f"{rc}\n{log}")

MOCK.reset()
MOCK.status["/api/crates/ferroly-derive/0.10.0"] = 500
rc, log, out, _ = drift(repo)
check("drift: crates.io API error fails with its own message",
      rc == 1 and "could not ask crates.io" in log, f"{rc}\n{log}")

notags = drift_repo([], version="0.1.0", version_age=600)
rc, log, out, _ = drift(notags, GRACE_HOURS="24")
check("drift: no v* tags -> nothing to check, pass", rc == 0 and "No v* tags" in log, f"{rc}\n{log}")

# =============================================================================
# crates-io-drift-alert.yml
# =============================================================================
ALERT = step_script("crates-io-drift-alert.yml", "alert", "Open or update the tracking issue")
AENV = {"RUN_URL": "https://example.invalid/run/1", "TITLE": "crates.io drift: a release is not on crates.io"}
rc, log, out, _ = run(ALERT, AENV)
calls = gh_calls()
check("alert: no open drift issue -> opens one linking the run (REL-8)",
      rc == 0 and len(calls) >= 2 and calls[0].startswith("issue list --state open")
      and calls[1].startswith("issue create --title crates.io drift: a release is not on crates.io")
      and "https://example.invalid/run/1" in "\n".join(calls[1:]), f"{rc} {calls}\n{log}")
rc, log, out, _ = run(ALERT, dict(AENV, STUB_GH_ISSUE="42"))
calls = gh_calls()
check("alert: an open drift issue -> comments on it instead of opening another",
      rc == 0 and len(calls) == 2 and calls[1] == "issue comment 42 --body Still failing: https://example.invalid/run/1",
      f"{rc} {calls}\n{log}")
rc, log, out, _ = run(ALERT, dict(AENV, STUB_GH_EXIT="1"))
check("alert: a failing gh issue list fails the job (no duplicate issue)",
      rc != 0 and len(gh_calls()) == 1, f"{rc} {gh_calls()}\n{log}")

print()
print(f"{len(FAILS)} failed" if FAILS else "all passed")
sys.exit(1 if FAILS else 0)
