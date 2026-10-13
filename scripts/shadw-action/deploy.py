#!/usr/bin/env python3
"""Deploy to Shadw from GitHub Actions using a standard shadw.yml.

Standard library only; shadw.yml is parsed with PyYAML when present, else a
strict minimal parser for the flat shadw.yml v1 shape (see schemas/shadw.schema.json).
Maps 1:1 onto POST /v1/git/deploy (fluid_core::GitDeployRequest) and polls
GET /v1/builds/:id until state is ready or error, like `shadw deploy --follow`.
"""
from __future__ import annotations

import json
import os
import re
import sys
import time
import urllib.error
import urllib.request
from typing import Any

SCHEMA_VERSION = 1
TOP_KEYS = {"version", "project", "root_dir", "branch", "target", "build", "env", "deploy"}
TARGETS = {"auto", "production", "preview"}
ENV_KEY = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*$")
PROJECT = re.compile(r"^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$")


class ConfigError(ValueError):
    pass


def _scalar(raw: str) -> Any:
    v = raw.strip()
    if v[:1] in "\"'" and v[-1:] == v[:1] and len(v) >= 2:
        return v[1:-1]
    low = v.lower()
    if low in {"true", "yes", "on"}:
        return True
    if low in {"false", "no", "off"}:
        return False
    if low in {"", "null", "~"}:
        return None
    if re.fullmatch(r"-?\d+", v):
        return int(v)
    return v


def parse_minimal_yaml(text: str) -> dict[str, Any]:
    """Two-level mappings of scalars only: exactly what shadw.yml v1 needs."""
    root: dict[str, Any] = {}
    current: dict[str, Any] | None = None
    for n, line in enumerate(text.splitlines(), 1):
        body = re.sub(r"\s+#.*$", "", line) if not line.lstrip().startswith("#") else ""
        if not body.strip():
            continue
        indent = len(body) - len(body.lstrip(" "))
        if "\t" in body[:indent + 1]:
            raise ConfigError(f"shadw.yml line {n}: tabs are not allowed")
        key, sep, rest = body.strip().partition(":")
        if not sep or not key:
            raise ConfigError(f"shadw.yml line {n}: expected 'key: value'")
        if indent == 0:
            if rest.strip():
                root[key] = _scalar(rest)
                current = None
            else:
                current = root[key] = {}
        elif current is not None:
            current[key] = _scalar(rest)
        else:
            raise ConfigError(f"shadw.yml line {n}: unexpected indentation")
    return root


def load_config(path: str) -> dict[str, Any]:
    if not path or not os.path.exists(path):
        return {}
    with open(path, encoding="utf-8") as fh:
        text = fh.read()
    if len(text) > 64 * 1024:
        raise ConfigError("shadw.yml exceeds 64 KiB")
    try:
        import yaml  # type: ignore
        data = yaml.safe_load(text) or {}
    except ImportError:
        data = parse_minimal_yaml(text)
    if not isinstance(data, dict):
        raise ConfigError("shadw.yml must be a mapping")
    return validate(data)


def validate(cfg: dict[str, Any]) -> dict[str, Any]:
    unknown = set(cfg) - TOP_KEYS
    if unknown:
        raise ConfigError(f"shadw.yml: unknown keys {sorted(unknown)}")
    version = cfg.get("version", SCHEMA_VERSION)
    if version != SCHEMA_VERSION:
        raise ConfigError(f"shadw.yml: unsupported version {version!r} (expected 1)")
    project = cfg.get("project")
    if project is not None and (not isinstance(project, str) or not PROJECT.match(project)):
        raise ConfigError("shadw.yml: project must be a lowercase DNS label (a-z, 0-9, -)")
    root_dir = cfg.get("root_dir")
    if root_dir is not None:
        if not isinstance(root_dir, str) or root_dir.startswith("/") or ".." in root_dir.split("/"):
            raise ConfigError("shadw.yml: root_dir must be a relative path inside the repo")
    target = cfg.get("target", "auto")
    if target not in TARGETS:
        raise ConfigError(f"shadw.yml: target must be one of {sorted(TARGETS)}")
    for section, allowed in (("build", {"cache"}), ("deploy", {"wait", "timeout_minutes"})):
        value = cfg.get(section) or {}
        if not isinstance(value, dict) or set(value) - allowed:
            raise ConfigError(f"shadw.yml: {section} accepts only {sorted(allowed)}")
    env = cfg.get("env") or {}
    if not isinstance(env, dict):
        raise ConfigError("shadw.yml: env must be a mapping")
    for k, v in env.items():
        if not ENV_KEY.match(str(k)) or isinstance(v, (dict, list)):
            raise ConfigError(f"shadw.yml: invalid env entry {k!r}")
    return cfg


def parse_env_lines(text: str) -> dict[str, str]:
    out: dict[str, str] = {}
    for line in (text or "").splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        key, sep, value = line.partition("=")
        if not sep or not ENV_KEY.match(key.strip()):
            raise ConfigError("env input: expected KEY=VALUE lines")
        out[key.strip()] = value
    return out


def _bool(value: str, default: bool) -> bool:
    if value is None or str(value).strip() == "":
        return default
    return str(value).strip().lower() in {"1", "true", "yes", "on"}


def build_request(cfg: dict[str, Any], e: dict[str, str]) -> tuple[dict[str, Any], dict[str, Any]]:
    body: dict[str, Any] = {"repo_url": e["SHADW_REPO_URL"], "creator": "github-actions"}
    branch = cfg.get("branch") or e.get("SHADW_BRANCH")
    if branch:
        body["branch"] = str(branch)
    commit = e.get("SHADW_COMMIT", "")
    if re.fullmatch(r"[0-9a-f]{40}", commit) and not cfg.get("branch"):
        body["commit"] = commit
    project = e.get("SHADW_PROJECT") or cfg.get("project")
    if project:
        if not PROJECT.match(project):
            raise ConfigError("project must be a lowercase DNS label")
        body["project"] = project
    if cfg.get("root_dir") not in (None, "", "."):
        body["root_dir"] = str(cfg["root_dir"]).strip("/")
    target = e.get("SHADW_TARGET") or cfg.get("target") or "auto"
    if target not in TARGETS:
        raise ConfigError(f"target must be one of {sorted(TARGETS)}")
    if target == "auto" and e.get("SHADW_EVENT") == "pull_request":
        target = "preview"
    if target != "auto":
        body["target"] = target
    if (cfg.get("build") or {}).get("cache") is False:
        body["use_cache"] = False
    env = {str(k): "" if v is None else str(v) for k, v in (cfg.get("env") or {}).items()}
    env.update(parse_env_lines(e.get("SHADW_EXTRA_ENV", "")))
    if env:
        body["env"] = env
    deploy = cfg.get("deploy") or {}
    opts = {
        "wait": _bool(e.get("SHADW_WAIT", ""), deploy.get("wait", True) is not False),
        "timeout": max(1, min(int(deploy.get("timeout_minutes", 20)), 120)) * 60,
    }
    return body, opts


class Shadw:
    def __init__(self, api: str, token: str, team: str = "") -> None:
        if not re.match(r"^https?://", api):
            raise ConfigError("api-url must be http(s)")
        self.api, self.token, self.team = api.rstrip("/"), token, team

    def call(self, method: str, path: str, body: dict[str, Any] | None = None) -> Any:
        headers = {"Accept": "application/json", "Authorization": f"Bearer {self.token}",
                   "User-Agent": "shadw-deploy-action/1"}
        if self.team:
            headers["x-hive-team"] = self.team
        data = None
        if body is not None:
            data = json.dumps(body).encode()
            headers["Content-Type"] = "application/json"
        req = urllib.request.Request(self.api + path, data=data, method=method, headers=headers)
        try:
            with urllib.request.urlopen(req, timeout=60) as resp:
                return json.loads(resp.read() or b"null")
        except urllib.error.HTTPError as err:
            detail = err.read(300).decode("utf-8", "replace").strip()
            hint = ""
            if err.code in (301, 302, 307, 308):
                hint = " (redirected; check api-url points at the Shadw API base, not the dashboard)"
            elif err.code == 401:
                hint = " (API key missing or rejected; check the token secret)"
            raise RuntimeError(f"{method} {path} -> HTTP {err.code}{hint}: {detail}") from None


def output(name: str, value: str) -> None:
    path = os.environ.get("GITHUB_OUTPUT")
    if path:
        with open(path, "a", encoding="utf-8") as fh:
            fh.write(f"{name}={value}\n")


def main() -> int:
    e = dict(os.environ)
    for key in ("SHADW_REPO_URL",):
        if not e.get(key):
            print(f"::error::{key} is not set", file=sys.stderr)
            return 2
    try:
        cfg = load_config(e.get("SHADW_CONFIG", "shadw.yml"))
        body, opts = build_request(cfg, e)
    except ConfigError as err:
        print(f"::error::{err}")
        return 2
    shown = dict(body)
    if "env" in shown:
        shown["env"] = {k: "***" for k in shown["env"]}
    print("Shadw deploy request:\n" + json.dumps(shown, indent=2))
    if _bool(e.get("SHADW_DRY_RUN", ""), False):
        output("state", "dry-run")
        return 0
    if not e.get("SHADW_TOKEN"):
        print("::error::token input is empty (set a SHADW_TOKEN repository secret)")
        return 2
    for value in body.get("env", {}).values():
        if value:
            print(f"::add-mask::{value}")
    if not e.get("SHADW_API_URL"):
        print("::error::api-url input is empty (set it to your Shadw API base, e.g. a SHADW_API_URL variable)")
        return 2
    hub = Shadw(e["SHADW_API_URL"],
                 e["SHADW_TOKEN"], e.get("SHADW_TEAM", ""))
    try:
        res = hub.call("POST", "/v1/git/deploy", body) or {}
    except RuntimeError as err:
        print(f"::error::{err}")
        return 1
    build_id, project = str(res.get("build_id", "")), str(res.get("project", ""))
    output("build-id", build_id)
    output("project", project)
    print(f"Deploy started: build {build_id or '?'} (project {project or '?'})")
    if not opts["wait"] or not build_id:
        output("state", "queued")
        return 0
    deadline, shown_lines, state, build = time.time() + opts["timeout"], 0, "", {}
    while time.time() < deadline:
        try:
            build = hub.call("GET", f"/v1/builds/{build_id}") or {}
        except RuntimeError as err:
            print(f"::warning::{err}")
            time.sleep(5)
            continue
        lines = build.get("lines") or []
        for line in lines[shown_lines:]:
            print(line.get("line", "") if isinstance(line, dict) else line)
        shown_lines = len(lines)
        state = str(build.get("state", ""))
        if state in {"ready", "error", "canceled", "cancelled"}:
            break
        time.sleep(3)
    url = str(build.get("url") or build.get("alias") or build.get("deployment_url") or "")
    output("state", state or "timeout")
    output("url", url)
    if state == "ready":
        print(f"Build {build_id} ready {url}".rstrip())
        if url and os.environ.get("GITHUB_STEP_SUMMARY"):
            with open(os.environ["GITHUB_STEP_SUMMARY"], "a", encoding="utf-8") as fh:
                fh.write(f"### Shadw deploy ready\n\n{project}: {url}\n")
        return 0
    print(f"::error::Shadw build {build_id} ended in state '{state or 'timeout'}'")
    return 1


if __name__ == "__main__":
    sys.exit(main())
