"""Bounded text inference for agents. No tools, shell execution or prompt persistence."""
import argparse
import ctypes
import datetime as dt
from decimal import Decimal, ROUND_CEILING, DecimalException
import json
import os
from pathlib import Path
import sqlite3
import sys
import time
import urllib.request
import uuid

ROOT = Path(__file__).resolve().parents[1]
OPENROUTER = "https://openrouter.ai/api/v1"
MAX_RESPONSE = 2 * 1024 * 1024

class Refusal(Exception):
    pass

class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        raise Refusal("Provider redirects are disabled")

def load(path):
    return json.loads(Path(path).read_text(encoding="utf-8-sig"))

def micros(value):
    amount = Decimal(str(value))
    if not amount.is_finite() or amount < 0:
        raise Refusal("Invalid money value")
    return int((amount * 1000000).to_integral_value(rounding=ROUND_CEILING))

def request_json(url, payload=None, key=None, timeout=45):
    headers = {"Content-Type": "application/json"}
    if key:
        headers["Authorization"] = "Bearer " + key
    request = urllib.request.Request(url, None if payload is None else json.dumps(payload).encode(), headers)
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
    with opener.open(request, timeout=timeout) as response:
        raw = response.read(MAX_RESPONSE + 1)
    if len(raw) > MAX_RESPONSE:
        raise Refusal("Provider response exceeds limit")
    return json.loads(raw)

def credential(reference):
    # Same credential-reference contract as runner-core/src/credentials.rs.
    target = reference.removeprefix("credential-store:")
    if not reference.startswith("credential-store:") or not target or len(target) > 260 or not all(c.isascii() and (c.isalnum() or c in ".:/_-") for c in target):
        raise Refusal("Invalid OS credential reference")
    if os.name != "nt":
        raise Refusal("Use OPENROUTER_API_KEY on this platform")
    from ctypes import wintypes as w
    class Credential(ctypes.Structure):
        _fields_ = [("Flags", w.DWORD), ("Type", w.DWORD), ("TargetName", w.LPWSTR), ("Comment", w.LPWSTR), ("LastWritten", w.FILETIME), ("CredentialBlobSize", w.DWORD), ("CredentialBlob", ctypes.POINTER(ctypes.c_ubyte)), ("Persist", w.DWORD), ("AttributeCount", w.DWORD), ("Attributes", ctypes.c_void_p), ("TargetAlias", w.LPWSTR), ("UserName", w.LPWSTR)]
    api = ctypes.WinDLL("Advapi32.dll")
    pointer = ctypes.POINTER(Credential)()
    api.CredReadW.argtypes = [w.LPCWSTR, w.DWORD, w.DWORD, ctypes.POINTER(ctypes.POINTER(Credential))]
    api.CredReadW.restype = w.BOOL
    api.CredFree.argtypes = [ctypes.c_void_p]
    if not api.CredReadW(target, 1, 0, ctypes.byref(pointer)):
        raise Refusal("OS credential unavailable")
    try:
        if not 0 < pointer.contents.CredentialBlobSize <= 4096:
            raise Refusal("Invalid credential size")
        value = ctypes.string_at(pointer.contents.CredentialBlob, pointer.contents.CredentialBlobSize).decode("utf-8").strip()
    finally:
        api.CredFree(pointer)
    if not value or any(ord(c) < 32 for c in value):
        raise Refusal("Invalid credential value")
    return value

def api_key(config):
    value = os.environ.get("OPENROUTER_API_KEY")
    if value:
        return value
    settings = load(config["settings_path"])
    profile = next((p for p in settings["providers"] if p["id"] == config["openrouter_provider_id"] and p["kind"] == "openRouter"), None)
    if not profile:
        raise Refusal("Configured OpenRouter profile unavailable")
    return credential(profile.get("secretRef", ""))

def route(policy, task, private=False):
    selected = dict(policy["routes"][task])
    if selected["provider"] not in {"ollama", "openrouter"}:
        raise Refusal("Unsupported route provider")
    if private and selected["provider"] != "ollama":
        raise Refusal("Private task requires an explicit local route; no cloud fallback")
    return selected

class Ledger:
    def __init__(self, path):
        path = Path(path).resolve()
        if path == ROOT or ROOT in path.parents:
            raise Refusal("Usage ledger must be outside the shared Fabrica checkout")
        path.parent.mkdir(parents=True, exist_ok=True)
        self.db = sqlite3.connect(path, timeout=15, isolation_level=None)
        self.db.execute("PRAGMA journal_mode=WAL")
        self.db.execute("CREATE TABLE IF NOT EXISTS calls(id TEXT PRIMARY KEY, day TEXT NOT NULL, provider TEXT NOT NULL, model TEXT NOT NULL, task TEXT NOT NULL, state TEXT NOT NULL, reserved_micros INTEGER NOT NULL, cost_micros INTEGER, input_tokens INTEGER, output_tokens INTEGER, elapsed_ms INTEGER, created_at TEXT NOT NULL)")
        if "reported_cost_usd" not in {row[1] for row in self.db.execute("PRAGMA table_info(calls)")}:
            self.db.execute("ALTER TABLE calls ADD COLUMN reported_cost_usd TEXT")

    def totals(self, day):
        row = self.db.execute("SELECT COUNT(*), COALESCE(SUM(CASE WHEN state='settled' THEN cost_micros ELSE reserved_micros END),0), COALESCE(SUM(input_tokens),0), COALESCE(SUM(output_tokens),0), SUM(CASE WHEN state!='settled' THEN 1 ELSE 0 END) FROM calls WHERE day=?", (day,)).fetchone()
        providers = {}
        for provider, inputs, outputs, cost, unresolved in self.db.execute("SELECT provider,COALESCE(SUM(input_tokens),0),COALESCE(SUM(output_tokens),0),COALESCE(SUM(CASE WHEN state='settled' THEN cost_micros ELSE reserved_micros END),0),SUM(CASE WHEN state!='settled' THEN 1 ELSE 0 END) FROM calls WHERE day=? GROUP BY provider", (day,)):
            providers[provider] = dict(reported_input_tokens=inputs, reported_output_tokens=outputs, committed_micros=cost, unresolved_calls=unresolved)
        reported_cost = sum((Decimal(value) for (value,) in self.db.execute("SELECT reported_cost_usd FROM calls WHERE day=? AND reported_cost_usd IS NOT NULL", (day,))), Decimal(0))
        return dict(reported_cost_usd=str(reported_cost), calls=row[0], committed_micros=row[1], reported_input_tokens=row[2], reported_output_tokens=row[3], unresolved_calls=row[4] or 0, by_provider=providers)

    def reserve(self, day, selected, task, amount, cap):
        self.db.execute("BEGIN IMMEDIATE")
        try:
            total = self.totals(day)["committed_micros"]
            if total + amount > cap:
                raise Refusal(f"Daily budget insufficient: committed/reserved ${total/1e6:.6f}; next ceiling ${amount/1e6:.6f}; cap ${cap/1e6:.2f}. Ask user about a task-specific increase.")
            identity = str(uuid.uuid4())
            self.db.execute("INSERT INTO calls(id,day,provider,model,task,state,reserved_micros,cost_micros,input_tokens,output_tokens,elapsed_ms,created_at) VALUES(?,?,?,?,?,'dispatched',?,NULL,NULL,NULL,NULL,?)", (identity, day, selected["provider"], selected["model"], task, amount, dt.datetime.now(dt.timezone.utc).isoformat()))
            self.db.execute("COMMIT")
            return identity
        except BaseException:
            self.db.execute("ROLLBACK")
            raise

    def settle(self, identity, cost, input_tokens, output_tokens, elapsed):
        # Missing cost or tokens retains the reservation; timeout/crash never becomes zero.
        valid_token = lambda x: isinstance(x, int) and not isinstance(x, bool) and x >= 0
        complete = cost is not None and valid_token(input_tokens) and valid_token(output_tokens)
        actual = micros(cost) if cost is not None else None
        ceiling = self.db.execute("SELECT reserved_micros FROM calls WHERE id=?", (identity,)).fetchone()[0]
        state = "settled" if complete else "unknown"
        if actual is not None and actual > ceiling:
            state = "overrun"
        self.db.execute("UPDATE calls SET state=?, reserved_micros=MAX(reserved_micros,?),cost_micros=?,input_tokens=?,output_tokens=?,elapsed_ms=?,reported_cost_usd=? WHERE id=?", (state, actual or 0, actual, input_tokens if valid_token(input_tokens) else None, output_tokens if valid_token(output_tokens) else None, elapsed, str(cost) if cost is not None else None, identity))
        return state

def cloud_quote(selected, max_output, get=request_json):
    catalog = get(OPENROUTER + "/models")
    model = next((m for m in catalog["data"] if m["id"] == selected["model"]), None)
    if not model:
        raise Refusal("Exact model unavailable in fresh catalog")
    prices = model["pricing"]
    # Tier overrides and additional modalities/fees need a richer quote; never silently underprice.
    if model.get("pricing_overrides") or prices.get("pricing_overrides"):
        raise Refusal("Tiered pricing requires a tier-aware adapter")
    for name, value in prices.items():
        if name not in {"prompt", "completion", "internal_reasoning", "input_cache_read", "input_cache_write", "input_cache_write_1h", "discount", "image", "audio", "input_audio_cache", "web_search"}:
            if isinstance(value, (str, int, float)) and Decimal(str(value)) != 0:
                raise Refusal("Additional provider charges are unsupported")
    input_rate, output_rate = Decimal(str(prices["prompt"])), Decimal(str(prices["completion"]))
    if min(input_rate, output_rate) < 0:
        raise Refusal("Invalid catalog price")
    envelope = selected["input_envelope"]
    if envelope + max_output > model["context_length"]:
        raise Refusal("Request exceeds model context")
    amount = micros(envelope * input_rate + max_output * output_rate)
    return amount, {"prompt": float(input_rate * 1000000), "completion": float(output_rate * 1000000), "request": 0}

def run(config, policy, task, prompt, private=False, get=request_json):
    selected = route(policy, task, private)
    max_output = selected["max_output_tokens"]
    # Conservative byte envelope, including a margin for a single message. Refuse rather than truncate.
    if not prompt.strip() or len(prompt.encode("utf-8")) + 512 > selected["input_envelope"]:
        raise Refusal("Prompt empty or exceeds bounded input envelope; split the task")
    day = dt.date.today().isoformat()  # Host-local calendar day, recorded explicitly.
    ledger = Ledger(config["ledger_path"])
    try:
        if ledger.db.execute("SELECT 1 FROM calls WHERE state='overrun' LIMIT 1").fetchone():
            raise Refusal("Prior provider overrun requires ledger review before further calls")
        cloud = selected["provider"] == "openrouter"
        if cloud:
            cap = micros(config["daily_budget_usd"])
            amount, ceilings = cloud_quote(selected, max_output, get)
            if amount > micros(config["per_call_budget_usd"]):
                raise Refusal("Per-call quote exceeds configured limit; ask user before raising it")
            key = api_key(config)
            url = OPENROUTER + "/chat/completions"
            payload = {"model": selected["model"], "messages": [{"role": "user", "content": prompt}], "max_tokens": max_output, "provider": {"max_price": ceilings, "require_parameters": True, "data_collection": "deny", "allow_fallbacks": False}}
        else:
            cap, amount, key = 0, 0, None
            # Local means loopback, not an arbitrary endpoint supplied by task text.
            url = "http://127.0.0.1:11434/api/generate"
            payload = {"model": selected["model"], "prompt": prompt, "stream": False, "options": {"num_ctx": selected["input_envelope"], "num_predict": max_output}}
        identity = ledger.reserve(day, selected, task, amount, cap if cloud else 10**18)
        start = time.monotonic()
        try:
            result = get(url, payload, key, timeout=selected["timeout_seconds"])
            elapsed = round((time.monotonic() - start) * 1000)
            if cloud:
                usage = result.get("usage") or {}
                input_tokens, output_tokens, cost = usage.get("prompt_tokens"), usage.get("completion_tokens"), usage.get("cost")
                choice = result.get("choices", [{}])[0]
                text = choice.get("message", {}).get("content")
                finished = choice.get("finish_reason") == "stop"
            else:
                input_tokens, output_tokens, cost = result.get("prompt_eval_count"), result.get("eval_count"), 0
                text = result.get("response")
                finished = result.get("done") is True and result.get("done_reason") != "length"
            state = ledger.settle(identity, cost, input_tokens, output_tokens, elapsed)
            totals = ledger.totals(day)
            warning = cloud and totals["committed_micros"] >= micros(config["daily_budget_usd"]) * .8
            return {"call_id": identity, "route": selected, "text": text, "complete": finished and isinstance(text, str) and bool(text.strip()), "billing_state": state, "usage": {"input_tokens": input_tokens, "output_tokens": output_tokens, "reported_cost_usd": cost, "source": "openrouter-response" if cloud else "ollama-response", "elapsed_ms": elapsed}, "day": day, "daily_totals": totals, "budget_nearing_limit": warning, "budget_note": "Ask user for a task-specific increase before exceeding the cap" if warning else None}
        except Exception as error:
            # Do not persist arbitrary response bodies, prompts, URLs, headers or secrets.
            ledger.db.execute("UPDATE calls SET state='unknown' WHERE id=?", (identity,))
            raise Refusal(f"Provider call failed ({type(error).__name__}); usage remains reserved, no retry. Call {identity}") from None
    finally:
        ledger.db.close()

def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["route", "run", "usage"])
    parser.add_argument("--config", default=os.environ.get("FABRICA_AGENT_CONFIG"))
    parser.add_argument("--policy", default=str(ROOT / "config/model-routes.json"))
    parser.add_argument("--task")
    parser.add_argument("--private", action="store_true")
    args = parser.parse_args(argv)
    try:
        policy = load(args.policy)
        if args.command == "route":
            result = route(policy, args.task, args.private)
        else:
            if not args.config:
                raise Refusal("Set FABRICA_AGENT_CONFIG or pass --config for private settings")
            config = load(args.config)
            if args.command == "usage":
                ledger = Ledger(config["ledger_path"])
                result = {"day": dt.date.today().isoformat(), "cap_usd": config["daily_budget_usd"], **ledger.totals(dt.date.today().isoformat())}
                ledger.db.close()
            else:
                prompt = sys.stdin.buffer.read(65537)
                if len(prompt) > 65536:
                    raise Refusal("Input exceeds 64 KiB")
                result = run(config, policy, args.task, prompt.decode("utf-8-sig"), args.private)
        print(json.dumps(result, ensure_ascii=True))
        return 0 if result.get("complete", True) and result.get("billing_state") not in {"unknown", "overrun"} else 3
    except (Refusal, KeyError, ValueError, OSError, DecimalException, TypeError, sqlite3.Error) as error:
        # Only our fixed refusal text is safe for output. Other errors may contain private paths/content.
        print(json.dumps({"error": str(error) if isinstance(error, Refusal) else type(error).__name__}))
        return 2

if __name__ == "__main__":
    raise SystemExit(main())
