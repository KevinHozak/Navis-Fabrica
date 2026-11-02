"""Windows shared-helper authority. No provider access, migration or auto-init.

Production callers always use the OS known-folder authority, never a config/env
override. Explicit roots are for trusted operator APIs and generated fixtures.
"""
import contextlib
import ctypes as C
from ctypes import wintypes as W
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import threading
import uuid
import importlib.util
_spec=importlib.util.spec_from_file_location('_helper_private',Path(__file__).with_name('portal_private_files.py'))
_private=importlib.util.module_from_spec(_spec);_spec.loader.exec_module(_private)
verify_private_path=_private.verify_private_path


class FenceError(ValueError):
    pass


def _known_folder(folder_id):
    if os.name != 'nt': raise FenceError('FENCE_WINDOWS_REQUIRED')
    shell = C.WinDLL('shell32')
    shell.SHGetFolderPathW.argtypes = [W.HWND, C.c_int, W.HANDLE, W.DWORD, W.LPWSTR]
    shell.SHGetFolderPathW.restype = C.c_long
    text = C.create_unicode_buffer(32768)
    if shell.SHGetFolderPathW(None, folder_id, None, 0, text):
        raise FenceError('FENCE_ROOT_UNAVAILABLE')
    path = Path(text.value)
    canonical(path)
    return path


def _present(path):
    # Permission/lookup ambiguity must not become an absent legacy authority.
    try:
        path.lstat()
        return True
    except FileNotFoundError:
        return False
    except OSError:
        raise FenceError('FENCE_NAMESPACE_UNAVAILABLE') from None


def default_root():
    profile = _known_folder(0x28)  # CSIDL_PROFILE, resolved by Windows, not env.
    local = _known_folder(0x1c)    # Previous Local AppData policy, detection only.
    parent = profile/'NavisFabrica-Portal'
    legacy = local/'NavisFabrica-Portal'
    if canonical(parent) == canonical(legacy):
        raise FenceError('FENCE_ROOT_CONFLICT')
    if _present(legacy):
        raise FenceError('FENCE_LEGACY_ROOT_PRESENT')
    root = parent/'accounting-authority'
    for path in (parent, root):
        if not _present(path):
            continue
        if path.lstat().st_file_attributes & 0x400 or not path.is_dir():
            raise FenceError('FENCE_ROOT_CONFLICT')
        verify_private_path(path)
    return root


def canonical(path):
    path = Path(path)
    if not path.is_absolute() or str(path).startswith('\\\\'):
        raise FenceError('FENCE_LOCAL_ABSOLUTE_PATH_REQUIRED')
    return os.path.normcase(os.path.abspath(path))


def identity(path):
    verify_private_path(path); verify_private_path(Path(path).parent)
    for suffix in ('-wal','-shm','-journal'):
        sidecar=Path(str(path)+suffix)
        if sidecar.exists():verify_private_path(sidecar)
    info = Path(path).stat()
    if not Path(path).is_file() or not info.st_ino:
        raise FenceError('FENCE_LEDGER_IDENTITY_INVALID')
    return [info.st_dev, info.st_ino]


class Overlapped(C.Structure):
    _fields_ = [('internal', C.c_size_t), ('high', C.c_size_t),
                ('offset', W.DWORD), ('offset_high', W.DWORD), ('event', W.HANDLE)]


class Lock:
    def __init__(self, path, exclusive=True):
        verify_private_path(path)
        k = self.k = C.WinDLL('kernel32', use_last_error=True)
        k.CreateFileW.argtypes = [W.LPCWSTR,W.DWORD,W.DWORD,C.c_void_p,W.DWORD,W.DWORD,W.HANDLE]
        k.CreateFileW.restype = W.HANDLE
        k.LockFileEx.argtypes = [W.HANDLE,W.DWORD,W.DWORD,W.DWORD,W.DWORD,C.POINTER(Overlapped)]
        k.LockFileEx.restype = W.BOOL
        k.CloseHandle.argtypes = [W.HANDLE]; k.CloseHandle.restype = W.BOOL
        self.handle = k.CreateFileW(str(path),0xc0000000,3,None,3,0x80,None)
        if self.handle == W.HANDLE(-1).value:
            self.handle = None; raise FenceError('FENCE_LOCK_UNAVAILABLE')
        overlap = Overlapped()
        if not k.LockFileEx(self.handle,1 | (2 if exclusive else 0),0,1,0,C.byref(overlap)):
            self.close(); raise FenceError('FENCE_BUSY')

    def close(self):
        if self.handle:
            self.k.CloseHandle(self.handle); self.handle = None

    def __enter__(self): return self
    def __exit__(self, *_): self.close()


def logical_digest(path):
    """Read an existing protected DB; never create/repair/checkpoint it."""
    verify_private_path(path)
    if Path(path).stat().st_size>64*1024*1024:raise FenceError('FENCE_LEDGER_REVIEW_BOUND')
    db = sqlite3.connect(Path(path).as_uri()+'?mode=ro',uri=True)
    try:
        db.execute('BEGIN')
        if db.execute('PRAGMA page_count').fetchone()[0]*db.execute('PRAGMA page_size').fetchone()[0]>64*1024*1024:
            raise FenceError('FENCE_LEDGER_REVIEW_BOUND')
        schema = db.execute('SELECT type,name,sql FROM sqlite_master ORDER BY type,name').fetchall()
        if not any(kind == 'table' and name == 'calls' for kind,name,_ in schema):
            raise FenceError('FENCE_ACCOUNTING_SCHEMA_REQUIRED')
        if db.execute('PRAGMA integrity_check').fetchone()[0] != 'ok':
            raise FenceError('FENCE_SNAPSHOT_INVALID')
        def encode(row):
            return json.dumps([['blob',x.hex()] if isinstance(x,bytes) else [type(x).__name__,x]
                               for x in row],ensure_ascii=True,separators=(',',':'),allow_nan=False)
        digest = hashlib.sha256(encode(schema).encode())
        row_count=0;encoded_bytes=0
        for kind,name,_ in schema:
            if kind != 'table': continue
            rows=[]
            for row in db.execute('SELECT * FROM "'+name.replace('"','""')+'"'):
                value=encode(row);row_count+=1;encoded_bytes+=len(value)
                if row_count>1000000 or encoded_bytes>128*1024*1024:raise FenceError('FENCE_LEDGER_REVIEW_BOUND')
                rows.append(value)
            rows.sort()
            digest.update(json.dumps([name,rows],separators=(',',':')).encode())
        return digest.hexdigest()
    finally: db.close()


def create_private(path):
    # Reuse the same OS owner/DACL primitives as the bootstrap verifier.
    c,w,api,kernel = _private._windows()
    # portal_private_files intentionally exposes read-only API signatures only.
    api.ConvertStringSecurityDescriptorToSecurityDescriptorW.argtypes = [w.LPCWSTR,w.DWORD,c.POINTER(c.c_void_p),c.POINTER(w.DWORD)]
    api.ConvertStringSecurityDescriptorToSecurityDescriptorW.restype = w.BOOL
    kernel.CreateDirectoryW.argtypes = [w.LPCWSTR,c.c_void_p];kernel.CreateDirectoryW.restype = w.BOOL
    sid = _private._current_sid(c,w,api,kernel); descriptor=c.c_void_p()
    sddl='D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;'+sid+')'
    if not api.ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl,1,c.byref(descriptor),None):
        raise FenceError('FENCE_PRIVATE_DIRECTORY_FAILED')
    class Attributes(c.Structure):
        _fields_=[('length',w.DWORD),('descriptor',c.c_void_p),('inherit',w.BOOL)]
    attr=Attributes(c.sizeof(Attributes),descriptor,False)
    try:
        if not kernel.CreateDirectoryW(str(path),c.byref(attr)):
            raise FenceError('FENCE_ROOT_MUST_BE_NEW')
    finally:kernel.LocalFree(descriptor)
    verify_private_path(path)


class Fence:
    def __init__(self, root=None):
        self._canonical_namespace = root is None
        self.root = Path(root) if root is not None else default_root()

    def _check_namespace(self):
        # Recheck existing sessions too: appearance of old state blocks all
        # supported admissions, not just the next process constructor.
        if self._canonical_namespace and canonical(default_root()) != canonical(self.root):
            raise FenceError('FENCE_ROOT_CONFLICT')

    def control(self):
        self._check_namespace()
        verify_private_path(self.root)
        return Lock(self.root/'control.lock')

    def load(self):
        path=self.root/'authority.json';verify_private_path(path)
        if path.stat().st_size>65536:raise FenceError('FENCE_STATE_INVALID')
        def unique(pairs):
            result={}
            for key,value in pairs:
                if key in result:raise FenceError('FENCE_STATE_INVALID')
                result[key]=value
            return result
        state=json.loads(path.read_text(encoding='utf-8'),object_pairs_hook=unique)
        if (set(state)!={'schema','revision','epoch','phase','path','identity','retired'}
            or type(state['schema']) is not int or state['schema']!=1 or state['phase'] not in ('active','paused')
            or type(state['revision']) is not int or state['revision']<1
            or type(state['epoch']) is not int or state['epoch']<1
            or not isinstance(state['path'],str) or canonical(state['path'])!=state['path']
            or not isinstance(state['identity'],list) or len(state['identity'])!=2
            or any(type(x) is not int for x in state['identity'])
            or not isinstance(state['retired'],list) or len(state['retired'])>128
            or any(not isinstance(x,str) or canonical(x)!=x for x in state['retired'])
            or state['path'] in state['retired']):raise FenceError('FENCE_STATE_INVALID')
        return state

    def write(self,state):
        temp=self.root/('pending-'+uuid.uuid4().hex+'.json')
        with temp.open('xb') as f:
            f.write(json.dumps(state,sort_keys=True,separators=(',',':')).encode());f.flush();os.fsync(f.fileno())
        verify_private_path(temp)
        k=C.WinDLL('kernel32',use_last_error=True)
        k.MoveFileExW.argtypes=[W.LPCWSTR,W.LPCWSTR,W.DWORD];k.MoveFileExW.restype=W.BOOL
        if not k.MoveFileExW(str(temp),str(self.root/'authority.json'),9):
            raise FenceError('FENCE_STATE_COMMIT_FAILED')

    def initialize(self,ledger):
        """Operator only: existing protected accounting; always starts paused."""
        self._check_namespace()
        path=canonical(ledger);file_id=identity(path);logical_digest(path)
        create_private(self.root)
        for name in ('control.lock','writers.lock'):
            with (self.root/name).open('xb') as f:f.write(b'0');f.flush();os.fsync(f.fileno())
        with self.control():
            self.write(dict(schema=1,revision=1,epoch=1,phase='paused',path=path,identity=file_id,retired=[]))

    def session(self,ledger):
        path=canonical(ledger)
        with self.control():
            state=self.load()
            self.check(state,path)
            lease=Lock(self.root/'writers.lock',exclusive=False)
            return Session(self,lease,path,state['epoch'],state['identity'])

    @staticmethod
    def check(state,path,epoch=None,file_id=None):
        if state['phase']!='active':raise FenceError('FENCE_PAUSED')
        if path!=state['path'] or path in state['retired']:raise FenceError('FENCE_STALE_PATH')
        if epoch is not None and epoch!=state['epoch']:raise FenceError('FENCE_STALE_EPOCH')
        current=identity(path)
        if current!=state['identity'] or (file_id is not None and current!=file_id):
            raise FenceError('FENCE_LEDGER_REPLACED')

    def pause(self,expected_revision):
        if type(expected_revision) is not int or expected_revision<1:raise FenceError('FENCE_REVISION_CONFLICT')
        with self.control():
            state=self.load()
            if state['revision']!=expected_revision:raise FenceError('FENCE_REVISION_CONFLICT')
            state.update(phase='paused',revision=state['revision']+1);self.write(state)
            return state

    def resume(self,expected_revision):
        if type(expected_revision) is not int or expected_revision<1:raise FenceError('FENCE_REVISION_CONFLICT')
        with self.control(),Lock(self.root/'writers.lock'):
            state=self.load()
            if state['revision']!=expected_revision or state['phase']!='paused':
                raise FenceError('FENCE_REVISION_CONFLICT')
            if identity(state['path'])!=state['identity']:raise FenceError('FENCE_LEDGER_REPLACED')
            state.update(phase='active',revision=state['revision']+1);self.write(state)
            return state

    def select_snapshot(self,destination,expected_revision):
        """Bind a pre-existing identical snapshot, never copy/migrate/resume it."""
        if type(expected_revision) is not int or expected_revision<1:raise FenceError('FENCE_REVISION_CONFLICT')
        path=canonical(destination)
        with self.control(),Lock(self.root/'writers.lock'):
            state=self.load()
            if state['revision']!=expected_revision or state['phase']!='paused':
                raise FenceError('FENCE_REVISION_CONFLICT')
            if path==state['path'] or path in state['retired']:raise FenceError('FENCE_RETIRED_PATH')
            old_id=identity(state['path']);new_id=identity(path)
            if old_id!=state['identity'] or new_id==old_id:raise FenceError('FENCE_LEDGER_REPLACED')
            if logical_digest(state['path'])!=logical_digest(path):raise FenceError('FENCE_SNAPSHOT_MISMATCH')
            if len(state['retired'])>=128:raise FenceError('FENCE_RETIRED_PATH_LIMIT')
            state['retired'].append(state['path'])
            state.update(path=path,identity=new_id,epoch=state['epoch']+1,revision=state['revision']+1)
            self.write(state);return state


class Session:
    def __init__(self,fence,lease,path,epoch,file_id):
        self.fence=fence;self.lease=lease;self.path=path;self.epoch=epoch;self.file_id=file_id
        self.local=threading.local()

    @contextlib.contextmanager
    def admission(self):
        if not self.lease:raise FenceError('FENCE_SESSION_CLOSED')
        with self.fence.control():
            Fence.check(self.fence.load(),self.path,self.epoch,self.file_id)
            self.local.admitted=True
            try:yield
            finally:self.local.admitted=False

    def authorize(self,action,table,*_):
        if action==sqlite3.SQLITE_INSERT and table in ('calls','portal_operations','portal_runs'):
            try:
                if not getattr(self.local,'admitted',False):
                    with self.admission():pass
            except Exception:return sqlite3.SQLITE_DENY
        return sqlite3.SQLITE_OK

    def close(self):
        if self.lease:self.lease.close();self.lease=None


class Connection(sqlite3.Connection):
    def close(self):
        try:super().close()
        finally:
            if getattr(self,'fence_session',None):self.fence_session.close()
