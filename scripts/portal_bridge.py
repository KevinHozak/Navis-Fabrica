"""Explicit Portal integration bootstrap; no network or credential access at import."""
import hashlib
import importlib
import json
from pathlib import Path
import re
import sys
from portal_private_files import verify_private_path


def read_profile(path):
    with Path(path).open("rb") as source:
        raw=source.read(65537)
    if len(raw)>65536: raise ValueError("PORTAL_PROFILE_INVALID")
    def unique(pairs):
        out={}
        for key,value in pairs:
            if key in out: raise ValueError("PORTAL_PROFILE_INVALID")
            out[key]=value
        return out
    return json.loads(raw,object_pairs_hook=unique,parse_constant=lambda _: (_ for _ in ()).throw(ValueError()))


def run(helper, profile_path, config_path, prompt, operation_id, run_id):
    try:
        for path in (Path(profile_path).parent,profile_path,Path(config_path).parent,config_path):
            verify_private_path(path)
        profile=read_profile(profile_path)
        root=Path(profile["portal_release_root"])
        expected=profile["portal_release_id"]
        if not root.is_absolute() or not re.fullmatch(r"[0-9a-f]{64}",expected):
            raise ValueError()
        verify_private_path(root)
        verify_private_path(root/"manifest.json")
        manifest=read_profile(root/"manifest.json")
        encoded=json.dumps(manifest,sort_keys=True,separators=(",",":")).encode()
        if hashlib.sha256(encoded).hexdigest()!=expected: raise ValueError()
        entries=manifest["files"]
        if not isinstance(entries,dict) or not 1<=len(entries)<=64: raise ValueError()
        total=0
        for name,sha in entries.items():
            if not re.fullmatch(r"(?:(?:portal_gateway|portal_client)/[a-z_]+\.py|qualification/deepseek-v41/[A-Za-z_.]+|bootstrap\.py)",name):
                raise ValueError()
            target=root/name
            verify_private_path(target)
            for parent in target.parents:
                if parent==root: break
                verify_private_path(parent)
            if target.is_symlink(): raise ValueError()
            total+=target.stat().st_size
            if total>16*1024*1024: raise ValueError()
            if hashlib.sha256(target.read_bytes()).hexdigest()!=sha: raise ValueError()
        for name,module in list(sys.modules.items()):
            if name in {"portal_gateway","portal_client"} or name.startswith(("portal_gateway.","portal_client.")):
                path=getattr(module,"__file__",None)
                if path is None or not Path(path).resolve().is_relative_to(root.resolve()): raise ValueError()
        sys.path.insert(0,str(root))
        private=importlib.import_module("portal_gateway.private_files")
        private.verify_private_path(root)
        private.verify_private_path(profile_path)
        private.verify_private_path(config_path)
        adapter=importlib.import_module("portal_gateway.fabrica")
        return adapter.run(profile,helper.load(config_path),prompt,operation_id,run_id,
            ledger_factory=helper.Ledger,credential_reader=helper.credential,profile_path=profile_path)
    except KeyboardInterrupt:
        return {"complete":False,"billing_state":"unknown","code":"PORTAL_CANCELLED_OR_UNKNOWN","cost_usd":None}
    except Exception:
        raise helper.Refusal("Portal integration refused; no direct-provider fallback or automatic retry") from None
