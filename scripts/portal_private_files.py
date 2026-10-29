"""Read-only Portal private-path verifier snapshot for the Fabrica bootstrap.

Source: Navis-Portal/portal_gateway/private_files.py; no credential/provider loader.
"""
from __future__ import annotations

import os
from pathlib import Path

class CredentialError(RuntimeError):
    """Fixed private-file refusal codes; no provider dependency."""
    pass


def _windows():
    if os.name != "nt":
        raise CredentialError("WINDOWS_PRIVATE_FILES_REQUIRED")
    import ctypes as c
    from ctypes import wintypes as w
    api = c.WinDLL("advapi32", use_last_error=True)
    kernel = c.WinDLL("kernel32", use_last_error=True)
    pointer = c.c_void_p
    api.GetNamedSecurityInfoW.argtypes = [w.LPWSTR, c.c_int, w.DWORD] + [c.POINTER(pointer)] * 5
    api.GetNamedSecurityInfoW.restype = w.DWORD
    api.ConvertSidToStringSidW.argtypes = [pointer, c.POINTER(w.LPWSTR)]
    api.ConvertSidToStringSidW.restype = w.BOOL
    api.OpenProcessToken.argtypes = [w.HANDLE, w.DWORD, c.POINTER(w.HANDLE)]
    api.OpenProcessToken.restype = w.BOOL
    api.GetTokenInformation.argtypes = [w.HANDLE, c.c_int, pointer, w.DWORD, c.POINTER(w.DWORD)]
    api.GetTokenInformation.restype = w.BOOL
    api.GetAce.argtypes = [pointer, w.DWORD, c.POINTER(pointer)]
    api.GetAce.restype = w.BOOL
    api.ConvertStringSecurityDescriptorToSecurityDescriptorW.argtypes = [w.LPCWSTR, w.DWORD,
                                                                        c.POINTER(pointer), c.POINTER(w.DWORD)]
    api.ConvertStringSecurityDescriptorToSecurityDescriptorW.restype = w.BOOL
    kernel.GetCurrentProcess.argtypes = []
    kernel.GetCurrentProcess.restype = w.HANDLE
    kernel.CloseHandle.argtypes = [w.HANDLE]
    kernel.CloseHandle.restype = w.BOOL
    kernel.LocalFree.argtypes = [pointer]
    kernel.LocalFree.restype = pointer
    kernel.CreateDirectoryW.argtypes = [w.LPCWSTR, pointer]
    kernel.CreateDirectoryW.restype = w.BOOL
    return c, w, api, kernel


def _sid_text(c, w, api, kernel, sid):
    if not sid:
        raise CredentialError("PRIVATE_FILE_PERMISSIONS_REQUIRED")
    text = w.LPWSTR()
    if not api.ConvertSidToStringSidW(sid, c.byref(text)):
        raise CredentialError("PRIVATE_FILE_PERMISSIONS_REQUIRED")
    try:
        return text.value
    finally:
        kernel.LocalFree(c.cast(text, c.c_void_p))


def _current_sid(c, w, api, kernel):
    token = w.HANDLE()
    if not api.OpenProcessToken(kernel.GetCurrentProcess(), 8, c.byref(token)):
        raise CredentialError("PRIVATE_FILE_PERMISSIONS_REQUIRED")
    try:
        length = w.DWORD()
        api.GetTokenInformation(token, 1, None, 0, c.byref(length))
        if not 0 < length.value < 65536:
            raise CredentialError("PRIVATE_FILE_PERMISSIONS_REQUIRED")
        buffer = c.create_string_buffer(length.value)
        if not api.GetTokenInformation(token, 1, buffer, length.value, c.byref(length)):
            raise CredentialError("PRIVATE_FILE_PERMISSIONS_REQUIRED")
        sid = c.cast(buffer, c.POINTER(c.c_void_p))[0]
        return _sid_text(c, w, api, kernel, sid)
    finally:
        kernel.CloseHandle(token)


def verify_private_path(path: str | Path):
    """Allow only current account, LocalSystem and built-in administrators.

    Reject reparse points, null DACLs and unfamiliar ACE types. Do not repair ACLs.
    The operator must also secure ancestors against replacement by untrusted users.
    """
    c, w, api, kernel = _windows()
    path = Path(path).absolute()
    if str(path).startswith("\\\\"):
        raise CredentialError("PRIVATE_FILE_LOCAL_PATH_REQUIRED")
    try:
        # Resolve neither symlinks nor junctions: reject them, including ancestors.
        for part in (path, *path.parents):
            if part.lstat().st_file_attributes & 0x400:
                raise CredentialError("PRIVATE_FILE_REPARSE_POINT")
    except OSError:
        raise CredentialError("PRIVATE_FILE_UNAVAILABLE") from None
    allowed = {_current_sid(c, w, api, kernel), "S-1-5-18", "S-1-5-32-544"}
    owner, acl, descriptor = c.c_void_p(), c.c_void_p(), c.c_void_p()
    result = api.GetNamedSecurityInfoW(str(path), 1, 5, c.byref(owner), None,
                                      c.byref(acl), None, c.byref(descriptor))
    if result:
        raise CredentialError("PRIVATE_FILE_PERMISSIONS_REQUIRED")
    try:
        if not acl or _sid_text(c, w, api, kernel, owner) not in allowed:
            raise CredentialError("PRIVATE_FILE_PERMISSIONS_REQUIRED")
        class ACL(c.Structure):
            _fields_ = [("revision", w.BYTE), ("reserved", w.BYTE), ("size", w.WORD),
                        ("count", w.WORD), ("reserved2", w.WORD)]
        header = c.cast(acl, c.POINTER(ACL)).contents
        if not header.count:
            raise CredentialError("PRIVATE_FILE_PERMISSIONS_REQUIRED")
        for index in range(header.count):
            ace = c.c_void_p()
            if not api.GetAce(acl, index, c.byref(ace)):
                raise CredentialError("PRIVATE_FILE_PERMISSIONS_REQUIRED")
            ace_type = c.cast(ace, c.POINTER(w.BYTE))[0]
            if ace_type == 1:  # Deny ACE cannot widen access.
                continue
            if ace_type != 0:
                raise CredentialError("PRIVATE_FILE_PERMISSIONS_REQUIRED")
            # ACCESS_ALLOWED_ACE: 4-byte header, 4-byte mask, then SID.
            sid = c.c_void_p(ace.value + 8)
            if _sid_text(c, w, api, kernel, sid) not in allowed:
                raise CredentialError("PRIVATE_FILE_PERMISSIONS_REQUIRED")
    finally:
        kernel.LocalFree(descriptor)
