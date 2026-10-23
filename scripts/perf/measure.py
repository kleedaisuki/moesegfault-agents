"""Bounded staging journeys; writes metrics, never prompts, tokens or model output."""
from __future__ import annotations

import argparse
import datetime as dt
import importlib.metadata
import json
import math
import os
from pathlib import Path
import platform
import subprocess
import sys
import time
from urllib.parse import urlsplit
import uuid

import httpx
from openai import OpenAI

ROOT = Path(__file__).resolve().parents[2]


def staging_origin(value):
    url = urlsplit(value)
    local = url.hostname in {"127.0.0.1", "::1"} and url.scheme == "http"
    staging = url.hostname == "agents-staging.moesegfault.dev" and url.scheme == "https"
    if not (local or staging) or url.username or url.password or url.path not in {"", "/"} or url.query or url.fragment:
        raise argparse.ArgumentTypeError("Use the staging HTTPS origin or explicit HTTP loopback")
    return value.rstrip("/")


def summary(values):
    ordered = sorted(values)
    if not ordered:
        return {}
    return {
        "count": len(ordered), "min_ms": ordered[0],
        "p50_ms": ordered[math.ceil(len(ordered) * .5) - 1],
        "p95_ms": ordered[math.ceil(len(ordered) * .95) - 1],
        "max_ms": ordered[-1],
    }


def elapsed(start):
    return round((time.perf_counter() - start) * 1000, 3)


def stream_turn(sdk, agent, session=None, prompt="Reply with exactly: ready", on_session=None):
    start = time.perf_counter()
    times = {"first_event_ms": None, "first_text_ms": None}
    chunks, characters, usage, finish = 0, 0, None, False
    headers = {"x-agents-session": session} if session else {}
    with sdk.chat.completions.with_streaming_response.create(
        model=agent, messages=[{"role": "user", "content": prompt}],
        stream=True, extra_headers=headers,
    ) as response:
        times["headers_ms"] = elapsed(start)
        session = response.headers.get("x-agents-session")
        if not session:
            raise RuntimeError("missing_session_header")
        if on_session:
            on_session(session)
        for chunk in response.parse():
            chunks += 1
            if times["first_event_ms"] is None:
                times["first_event_ms"] = elapsed(start)
            for choice in chunk.choices:
                content = choice.delta.content
                if content:
                    if times["first_text_ms"] is None:
                        times["first_text_ms"] = elapsed(start)
                    characters += len(content)
                finish = finish or choice.finish_reason is not None
            if chunk.usage is not None:
                details = chunk.usage.prompt_tokens_details
                usage = {
                    "prompt_tokens": chunk.usage.prompt_tokens,
                    "completion_tokens": chunk.usage.completion_tokens,
                    "cached_tokens": details.cached_tokens if details else None,
                }
    if not finish or not characters:
        raise RuntimeError("incomplete_stream")
    return session, {**times, "total_ms": elapsed(start), "chunks": chunks,
                     "characters": characters, "usage": usage}


def api_json(http, method, path, body=None):
    response = http.request(method, path, json=body)
    if not response.is_success:
        # Error bodies may contain user data. Only status is an artifact.
        raise RuntimeError("api_status_" + str(response.status_code))
    return response.json() if response.content else None


def management(http, samples):
    results = {}
    for path in ["/v1/account", "/v1/agents", "/v1/sessions", "/v1/models"]:
        start = time.perf_counter()
        api_json(http, "GET", path)
        cold = elapsed(start)
        values = []
        for _ in range(samples):
            start = time.perf_counter()
            api_json(http, "GET", path)
            values.append(elapsed(start))
        results[path] = {"first_request_ms": cold, "warm_ms": values, "summary": summary(values)}
    return results


def inference(http, sdk, args, report):
    config = {
        "name": "perf-" + uuid.uuid4().hex[:12],
        "tools": ["time", "bash"] if args.workspace else ["time"],
        "workspace": args.workspace,
        "model": {"provider": args.provider, "model": args.model},
        "instructions": "Follow the request precisely. Keep answers short. Use time or bash whenever explicitly requested. Never invent tool results.",
        "compaction_percent": 80,
    }
    agent = api_json(http, "POST", "/v1/agents", config)["id"]
    sessions = []
    report["inference"] = {"model": args.model, "provider": args.provider, "samples": []}
    try:
        for _ in range(args.samples):
            session, cold = stream_turn(sdk, agent, on_session=sessions.append)
            _, warm = stream_turn(sdk, agent, session)
            _, tool = stream_turn(sdk, agent, session, "Call the time tool once and report the current UTC time briefly.")
            sample = {"cold_session": cold, "warm_turn": warm, "harness_tool": tool}
            if args.workspace:
                sample["workspace"] = workspace_turns(http, sdk, agent, session)
            report["inference"]["samples"].append(sample)
            api_json(http, "DELETE", "/v1/sessions/" + session)
            sessions.remove(session)
        for flow in ["cold_session", "warm_turn", "harness_tool"]:
            report["inference"][flow] = {
                metric: summary([sample[flow][metric] for sample in report["inference"]["samples"]])
                for metric in ["headers_ms", "first_event_ms", "first_text_ms", "total_ms"]
            }
    finally:
        cleanup_failures = 0
        for session in sessions:
            try:
                api_json(http, "DELETE", "/v1/sessions/" + session)
            except Exception:
                cleanup_failures += 1
        try:
            api_json(http, "DELETE", "/v1/agents/" + agent)
        except Exception:
            cleanup_failures += 1
        report["cleanup_failures"] = cleanup_failures


def workspace_turns(http, sdk, agent, session):
    marker = uuid.uuid4().hex
    _, cold = stream_turn(sdk, agent, session, f"Use bash to execute exactly: printf {marker} > perf-state; cat perf-state")
    before = api_json(http, "GET", "/v1/sessions/" + session)
    _, warm = stream_turn(sdk, agent, session, "Use bash to execute exactly: cat perf-state")
    after = api_json(http, "GET", "/v1/sessions/" + session)
    assert_tool_marker(after, len(before.get("messages", [])), marker)
    start = time.perf_counter()
    api_json(http, "DELETE", "/v1/sessions/" + session)
    archive_ms = elapsed(start)
    start = time.perf_counter()
    api_json(http, "POST", "/v1/sessions/" + session + "/resume", {})
    resume_ms = elapsed(start)
    before = api_json(http, "GET", "/v1/sessions/" + session)
    _, restored = stream_turn(sdk, agent, session, "Use bash to execute exactly: cat perf-state")
    after = api_json(http, "GET", "/v1/sessions/" + session)
    assert_tool_marker(after, len(before.get("messages", [])), marker)
    return {"cold_bash": cold, "warm_bash": warm, "restored_bash": restored,
            "archive_ms": archive_ms, "resume_ms": resume_ms}


def assert_tool_marker(snapshot, start, marker):
    messages = snapshot.get("messages", [])[start:]
    if not any(message.get("role") == "tool" and marker in str(message.get("content")) for message in messages):
        raise RuntimeError("workspace_tool_result_missing")


def cli(binary, origin, token, samples):
    env = {**os.environ, "MAGENTS_API_KEY": token, "MAGENTS_API_URL": origin}
    results = {}
    for name, arguments in [("help", ["--help"]), ("agents_list", ["agents", "list"])]:
        values = []
        for _ in range(samples):
            start = time.perf_counter()
            outcome = subprocess.run([binary, *arguments], env=env, capture_output=True, timeout=60)
            if outcome.returncode:
                raise RuntimeError("cli_exit_" + str(outcome.returncode))
            values.append(elapsed(start))
        results[name] = {"total_ms": values, "summary": summary(values)}
    return results


def arguments():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--origin", type=staging_origin, default=os.getenv("MAGENTS_TEST_API_ORIGIN", "https://agents-staging.moesegfault.dev"))
    parser.add_argument("--samples", type=int, default=3, help="1-20 repeated journeys; each inference sample makes 3 billed turns")
    parser.add_argument("--inference", action="store_true", help="Explicitly authorize bounded real billed model calls")
    parser.add_argument("--workspace", action="store_true", help="With --inference, add 3 billed bash turns/sample: cold, warm, archive/restore")
    parser.add_argument("--provider", choices=["deepseek", "openrouter"], default="deepseek")
    parser.add_argument("--model", default="deepseek-flash")
    parser.add_argument("--cli", help="Built magents executable; measures help and authenticated agents list")
    parser.add_argument("--output", default=".cache/perf/latest.json")
    args = parser.parse_args()
    if not 1 <= args.samples <= 20:
        parser.error("--samples must be 1-20")
    if args.workspace and not args.inference:
        parser.error("--workspace requires --inference")
    output = (ROOT / args.output).resolve()
    if not output.is_relative_to(ROOT / ".cache"):
        parser.error("--output must stay in the repository .cache directory")
    args.output = output
    return args


def main():
    args = arguments()
    token = os.getenv("MAGENTS_TEST_API_KEY")
    if not token:
        raise SystemExit("Set MAGENTS_TEST_API_KEY for a dedicated staging account")
    report = {
        "schema": 1, "timestamp_utc": dt.datetime.now(dt.timezone.utc).isoformat(),
        "origin": args.origin, "platform": platform.platform(), "python": platform.python_version(),
        "openai": importlib.metadata.version("openai"), "httpx": httpx.__version__,
        "samples": args.samples, "concurrency": 1,
        "git_revision": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
        "notes": ["First management request is excluded from warm distribution; DNS/TLS coldness is not forced.",
                  "Small sample p95 is descriptive, not a statistically reliable SLO.",
                  "Cold session does not imply cold upstream provider cache or cold Worker isolate."],
    }
    try:
        with httpx.Client(base_url=args.origin, headers={"Authorization": "Bearer " + token}, timeout=180, follow_redirects=False) as http:
            report["management"] = management(http, args.samples)
            if args.inference:
                with OpenAI(api_key=token, base_url=args.origin + "/v1", timeout=180, max_retries=0) as sdk:
                    inference(http, sdk, args, report)
            if args.cli:
                report["cli"] = cli(args.cli, args.origin, token, args.samples)
    except Exception as error:
        report["error_type"] = type(error).__name__
        if isinstance(error, RuntimeError) and str(error).startswith(("api_status_", "cli_exit_", "missing_session_", "incomplete_stream", "workspace_tool_result_missing")):
            report["error_code"] = str(error)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    print(str(args.output.relative_to(ROOT)))
    return 1 if "error_type" in report or report.get("cleanup_failures") else 0


if __name__ == "__main__":
    sys.exit(main())
