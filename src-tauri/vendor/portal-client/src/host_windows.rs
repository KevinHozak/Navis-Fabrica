//! Metadata and handle operations only; no target memory, payload or credential reads.
use crate::{error, ClientError};
use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::net::TcpStream;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

type H = *mut c_void;
#[repr(C)]
struct Guid {
    a: u32,
    b: u16,
    c: u16,
    d: [u8; 8],
}
#[repr(C)]
#[derive(Default)]
struct Time {
    low: u32,
    high: u32,
}
#[link(name = "kernel32")]
extern "system" {
    fn GetCurrentProcess() -> H;
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> H;
    fn CloseHandle(h: H) -> i32;
    fn LocalFree(h: H) -> H;
    fn GetProcessTimes(h: H, a: *mut Time, b: *mut Time, c: *mut Time, d: *mut Time) -> i32;
    fn QueryFullProcessImageNameW(h: H, flags: u32, buf: *mut u16, size: *mut u32) -> i32;
    fn WaitForSingleObject(h: H, ms: u32) -> u32;
    fn GetCurrentProcessId() -> u32;
    fn GetNamedPipeServerProcessId(pipe: H, pid: *mut u32) -> i32;
    fn WaitNamedPipeW(name: *const u16, timeout: u32) -> i32;
    fn SetNamedPipeHandleState(pipe: H, mode: *const u32, max_collection: H, timeout: H) -> i32;
    fn ReadFile(pipe: H, buffer: H, count: u32, read: *mut u32, overlapped: H) -> i32;
    fn WriteFile(pipe: H, buffer: H, count: u32, written: *mut u32, overlapped: H) -> i32;
    fn CreateFileW(
        path: *const u16,
        access: u32,
        share: u32,
        sa: H,
        how: u32,
        flags: u32,
        template: H,
    ) -> H;
    fn PeekNamedPipe(
        h: H,
        buf: H,
        size: u32,
        read: *mut u32,
        available: *mut u32,
        left: *mut u32,
    ) -> i32;
    fn GetLastError() -> u32;
}
#[link(name = "ntdll")]
extern "system" {
    fn NtQueryInformationProcess(process: H, class: u32, info: H, size: u32, returned: *mut u32) -> i32;
}
#[link(name = "advapi32")]
extern "system" {
    fn OpenProcessToken(process: H, access: u32, token: *mut H) -> i32;
    fn GetTokenInformation(token: H, class: u32, buffer: H, size: u32, needed: *mut u32) -> i32;
    fn ConvertSidToStringSidW(sid: H, out: *mut *mut u16) -> i32;
    fn GetNamedSecurityInfoW(
        path: *const u16,
        kind: u32,
        info: u32,
        owner: *mut H,
        group: H,
        dacl: *mut H,
        sacl: H,
        descriptor: *mut H,
    ) -> u32;
    fn GetAce(acl: H, index: u32, ace: *mut H) -> i32;
}
#[link(name = "shell32")]
extern "system" {
    fn SHGetKnownFolderPath(id: *const Guid, flags: u32, token: H, out: *mut *mut u16) -> i32;
}
#[link(name = "ole32")]
extern "system" {
    fn CoTaskMemFree(p: H);
}

fn bad() -> ClientError {
    error("not_sent", "HOST_PROFILE_INVALID")
}
fn wide(p: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    p.as_os_str().encode_wide().chain(Some(0)).collect()
}
fn drive_absolute(path: &[u16]) -> bool {
    path.len() >= 3
        && ((path[0] >= b'A' as u16 && path[0] <= b'Z' as u16)
            || (path[0] >= b'a' as u16 && path[0] <= b'z' as u16))
        && path[1] == b':' as u16
        && path[2] == b'\\' as u16
}
fn unc_root(path: &[u16]) -> bool {
    let mut parts = path.split(|unit| *unit == b'\\' as u16);
    let server = parts.next().unwrap_or_default();
    let share = parts.next().unwrap_or_default();
    !server.is_empty()
        && !share.is_empty()
        && server[0] != b'.' as u16
        && server[0] != b'?' as u16
}
fn ascii_upper(unit: u16) -> u16 {
    if (b'a' as u16..=b'z' as u16).contains(&unit) {
        unit - (b'a' as u16 - b'A' as u16)
    } else {
        unit
    }
}
fn extended_filesystem_path(path: &Path) -> Result<Vec<u16>, ClientError> {
    use std::os::windows::ffi::OsStrExt;
    const SEP: u16 = b'\\' as u16;
    const EXTENDED: [u16; 4] = [SEP, SEP, b'?' as u16, SEP];
    const UNC: [u16; 4] = [b'U' as u16, b'N' as u16, b'C' as u16, SEP];
    let mut raw: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .map(|unit| if unit == b'/' as u16 { SEP } else { unit })
        .collect();
    if raw.contains(&0) {
        return Err(bad());
    }
    if raw.starts_with(&EXTENDED) {
        let tail = &raw[EXTENDED.len()..];
        let extended_unc = tail.len() >= UNC.len()
            && tail[..UNC.len()]
                .iter()
                .zip(UNC)
                .all(|(actual, expected)| ascii_upper(*actual) == expected);
        if !drive_absolute(tail) && !(extended_unc && unc_root(&tail[UNC.len()..])) {
            return Err(bad());
        }
    } else if drive_absolute(&raw) {
        raw.splice(0..0, EXTENDED);
    } else if raw.starts_with(&[SEP, SEP]) && unc_root(&raw[2..]) {
        let mut extended = Vec::with_capacity(raw.len() + 8);
        extended.extend_from_slice(&EXTENDED);
        extended.extend_from_slice(&UNC);
        extended.extend_from_slice(&raw[2..]);
        raw = extended;
    } else {
        return Err(bad());
    }
    raw.push(0);
    Ok(raw)
}
unsafe fn text(p: *const u16) -> Result<String, ClientError> {
    if p.is_null() {
        return Err(bad());
    }
    let mut n = 0;
    while n < 32768 && *p.add(n) != 0 {
        n += 1
    }
    if n == 32768 {
        return Err(bad());
    }
    String::from_utf16(std::slice::from_raw_parts(p, n)).map_err(|_| bad())
}
unsafe fn sid_text(sid: H) -> Result<String, ClientError> {
    let mut out = std::ptr::null_mut();
    if ConvertSidToStringSidW(sid, &mut out) == 0 {
        return Err(bad());
    }
    let result = text(out);
    LocalFree(out.cast());
    result
}
unsafe fn process_sid(process: H) -> Result<String, ClientError> {
    let mut token = std::ptr::null_mut();
    if OpenProcessToken(process, 8, &mut token) == 0 {
        return Err(bad());
    }
    let result = (|| {
        let mut needed = 0;
        GetTokenInformation(token, 1, std::ptr::null_mut(), 0, &mut needed);
        if needed == 0 || needed > 65536 {
            return Err(bad());
        }
        // Pointer-aligned TOKEN_USER buffer, with lifetime covering SID conversion.
        let mut buf = vec![0usize; (needed as usize + 7) / 8];
        if GetTokenInformation(token, 1, buf.as_mut_ptr().cast(), needed, &mut needed) == 0 {
            return Err(bad());
        }
        sid_text(*(buf.as_ptr() as *const H))
    })();
    CloseHandle(token);
    result
}

pub(crate) fn broker_pipe() -> Result<Option<String>, ClientError> {
    let required=std::env::var_os("NAVIS_PORTAL_LIFECYCLE_REQUIRED");
    let pipe=std::env::var_os("NAVIS_PORTAL_LIFECYCLE_PIPE");
    match (required,pipe) {
        (None,None)=>Ok(None),
        (Some(required),Some(pipe)) if required==std::ffi::OsStr::new("1")=>{
            let value=pipe.to_str().ok_or_else(bad)?;
            let prefix=r"\\.\pipe\NavisPortalLifecycle-v1-";
            let suffix=value.strip_prefix(prefix).ok_or_else(bad)?;
            if suffix.len()!=32||!suffix.bytes().all(|b|b.is_ascii_digit()||(b'a'..=b'f').contains(&b)) {return Err(bad());}
            Ok(Some(value.to_owned()))
        }
        _=>Err(error("not_sent","GUARDIAN_BROKER_UNAVAILABLE")),
    }
}

static BROKER_REQUESTS: AtomicU64 = AtomicU64::new(1);
fn broker_request_id()->String{
    let tick=SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    let serial=BROKER_REQUESTS.fetch_add(1,Ordering::Relaxed);
    let material=format!("{}:{}:{}",unsafe{GetCurrentProcessId()},tick,serial);
    super::digest(material.as_bytes())[..32].to_owned()
}

pub(crate) fn broker_request(
    pipe_name:&str, acquire:bool, deadline:Instant,
    cancel:&crate::Cancellation, dropped:&crate::Cancellation,
    registration:&super::Registration,
)->Result<serde_json::Value,ClientError>{
    let id=broker_request_id();
    let op=if acquire{"acquire"}else{"status"};
    let raw=super::broker_request_bytes(&id,op)?;
    let mut endpoint=pipe_name.encode_utf16().collect::<Vec<_>>();endpoint.push(0);
    let handle=loop{
        broker_check(deadline,cancel,dropped)?;
        let remaining=deadline.saturating_duration_since(Instant::now()).as_millis().clamp(1,250) as u32;
        unsafe{
            if WaitNamedPipeW(endpoint.as_ptr(),remaining)!=0{
                let h=CreateFileW(endpoint.as_ptr(),0xc0000000,0,std::ptr::null_mut(),3,0,std::ptr::null_mut());
                if !h.is_null()&&h as isize!=-1{break h;}
            }
        }
        if Instant::now()>=deadline{return Err(error("not_sent","GUARDIAN_BROKER_UNAVAILABLE"));}
    };
    let pipe=unsafe{File::from_raw_handle(handle)};
    let server=unsafe{verify_guardian_peer(pipe.as_raw_handle(),registration)?};
    let mode=1u32; // PIPE_NOWAIT; all reads are preceded by bounded PeekNamedPipe.
    if unsafe{SetNamedPipeHandleState(pipe.as_raw_handle(),&mode,std::ptr::null_mut(),std::ptr::null_mut())}==0{
        return Err(error("not_sent","GUARDIAN_BROKER_UNAVAILABLE"));
    }
    let packet=[(raw.len() as u32).to_le_bytes().as_slice(),raw.as_slice()].concat();
    if let Err(failure)=pipe_write(pipe.as_raw_handle(),&packet,deadline,cancel,dropped){
        return Err(failure);
    }
    let response=(||{
        let head=pipe_read_exact(pipe.as_raw_handle(),4,deadline,cancel,dropped)?;
        let size=u32::from_le_bytes(head.try_into().map_err(|_|bad())?) as usize;
        if !(1..=65536).contains(&size){return Err(error("not_sent","LIFECYCLE_UNRESOLVED"));}
        pipe_read_exact(pipe.as_raw_handle(),size,deadline,cancel,dropped)
    })();
    let response=match response{
        Ok(response)=>response,
        Err(failure)=>return Err(failure),
    };
    if unsafe{WaitForSingleObject(server.as_raw_handle(),0)}!=258{
        return Err(error("not_sent","LIFECYCLE_UNRESOLVED"));
    }
    super::broker_reply_frame(&response,&id)
}

fn broker_check(deadline:Instant,cancel:&crate::Cancellation,dropped:&crate::Cancellation)->Result<(),ClientError>{
    if cancel.is_cancelled()||dropped.is_cancelled(){Err(error("not_sent","HOST_CANCELLED"))}
    else if Instant::now()>=deadline{Err(error("not_sent","HOST_START_TIMEOUT"))}
    else{Ok(())}
}

fn pipe_write(pipe:H,bytes:&[u8],deadline:Instant,cancel:&crate::Cancellation,dropped:&crate::Cancellation)->Result<(),ClientError>{
    let mut offset=0;
    while offset<bytes.len(){
        broker_check(deadline,cancel,dropped)?;
        let mut written=0u32;
        let ok=unsafe{WriteFile(pipe,bytes[offset..].as_ptr() as H,(bytes.len()-offset) as u32,&mut written,std::ptr::null_mut())};
        if ok!=0&&written>0{offset+=written as usize;continue;}
        std::thread::sleep(Duration::from_millis(2));
    }
    Ok(())
}

fn pipe_read_exact(pipe:H,size:usize,deadline:Instant,cancel:&crate::Cancellation,dropped:&crate::Cancellation)->Result<Vec<u8>,ClientError>{
    let mut output=Vec::with_capacity(size);
    while output.len()<size{
        broker_check(deadline,cancel,dropped)?;
        let mut available=0u32;
        if unsafe{PeekNamedPipe(pipe,std::ptr::null_mut(),0,std::ptr::null_mut(),&mut available,std::ptr::null_mut())}==0{
            return Err(error("not_sent","LIFECYCLE_UNRESOLVED"));
        }
        if available==0{std::thread::sleep(Duration::from_millis(2));continue;}
        let take=(size-output.len()).min(available as usize);
        let mut chunk=vec![0u8;take];let mut read=0u32;
        if unsafe{ReadFile(pipe,chunk.as_mut_ptr() as H,take as u32,&mut read,std::ptr::null_mut())}==0||read==0{
            return Err(error("not_sent","LIFECYCLE_UNRESOLVED"));
        }
        output.extend_from_slice(&chunk[..read as usize]);
    }
    Ok(output)
}

unsafe fn verify_guardian_peer(pipe:H,registration:&super::Registration)->Result<File,ClientError>{
    #[repr(C)]struct BasicInfo{reserved:H,peb:H,reserved2:[H;2],pid:usize,parent_pid:usize}
    let fail=||error("not_sent","GUARDIAN_CALLER_REFUSED");
    let mut pid=0u32;
    if GetNamedPipeServerProcessId(pipe,&mut pid)==0||pid==0||pid==GetCurrentProcessId(){return Err(fail());}
    let mut app_basic=std::mem::MaybeUninit::<BasicInfo>::uninit();let mut app_returned=0u32;
    let app_status=NtQueryInformationProcess(GetCurrentProcess(),0,app_basic.as_mut_ptr().cast(),std::mem::size_of::<BasicInfo>() as u32,&mut app_returned);
    if app_status!=0||app_returned as usize!=std::mem::size_of::<BasicInfo>()||app_basic.assume_init().parent_pid!=pid as usize{return Err(fail());}
    let(mut app_created,mut app_exited,mut app_kernel,mut app_user)=(Time::default(),Time::default(),Time::default(),Time::default());
    if GetProcessTimes(GetCurrentProcess(),&mut app_created,&mut app_exited,&mut app_kernel,&mut app_user)==0{return Err(fail());}
    let app_created=((app_created.high as u64)<<32)|app_created.low as u64;
    let process=OpenProcess(0x100000|0x1000,0,pid);
    if process.is_null(){return Err(fail());}
    let process_file=File::from_raw_handle(process);
    if WaitForSingleObject(process,0)!=258{drop(process_file);return Err(fail());}
    let(mut created,mut exited,mut kernel,mut user)=(Time::default(),Time::default(),Time::default(),Time::default());
    if GetProcessTimes(process,&mut created,&mut exited,&mut kernel,&mut user)==0{drop(process_file);return Err(fail());}
    let created=((created.high as u64)<<32)|created.low as u64;
    let mut name=vec![0u16;32768];let mut count=name.len() as u32;
    if created==0||app_created==0||created>app_created||QueryFullProcessImageNameW(process,0,name.as_mut_ptr(),&mut count)==0{drop(process_file);return Err(fail());}
    let image=match String::from_utf16(&name[..count as usize]){Ok(image)=>image,Err(_)=>{drop(process_file);return Err(fail());}};
    if image.to_lowercase()!=registration.profile.runtime.executable.to_string_lossy().to_lowercase()
        ||process_sid(process).map_err(|_|fail())?!=registration.profile.sid
        ||super::digest(&std::fs::read(&image).map_err(|_|fail())?)!=registration.profile.runtime.sha256{
        drop(process_file);return Err(fail());
    }
    Ok(process_file)
}
pub(crate) fn sid() -> Result<String, ClientError> {
    unsafe { process_sid(GetCurrentProcess()) }
}
pub(crate) fn root() -> Result<PathBuf, ClientError> {
    unsafe {
        let id = Guid {
            a: 0x5e6c858f,
            b: 0x0e22,
            c: 0x4760,
            d: [0x9a, 0xfe, 0xea, 0x33, 0x17, 0xb6, 0x71, 0x73],
        };
        let mut out = std::ptr::null_mut();
        if SHGetKnownFolderPath(&id, 0, std::ptr::null_mut(), &mut out) != 0 {
            return Err(bad());
        }
        let result = text(out).map(|s| PathBuf::from(s).join("NavisFabrica-Portal"));
        CoTaskMemFree(out.cast());
        result
    }
}
fn no_links(path: &Path) -> Result<(), ClientError> {
    if !path.is_absolute() {
        return Err(bad());
    }
    for p in path.ancestors() {
        if std::fs::symlink_metadata(p)
            .map_err(|_| bad())?
            .file_attributes()
            & 0x400
            != 0
        {
            return Err(bad());
        }
    }
    Ok(())
}
pub(crate) fn private(path: &Path) -> Result<(), ClientError> {
    no_links(path)?;
    let current = sid()?;
    let allowed = [current.as_str(), "S-1-5-18", "S-1-5-32-544"];
    let name = extended_filesystem_path(path)?;
    unsafe {
        let mut owner = std::ptr::null_mut();
        let mut acl = std::ptr::null_mut();
        let mut descriptor = std::ptr::null_mut();
        if GetNamedSecurityInfoW(
            name.as_ptr(),
            1,
            5,
            &mut owner,
            std::ptr::null_mut(),
            &mut acl,
            std::ptr::null_mut(),
            &mut descriptor,
        ) != 0
        {
            return Err(bad());
        }
        let result = (|| {
            if acl.is_null() || !allowed.contains(&sid_text(owner)?.as_str()) {
                return Err(bad());
            }
            // ACL header: revision/reserved/size/count/reserved.
            let count = *(acl.cast::<u8>().add(4) as *const u16) as u32;
            for i in 0..count {
                let mut ace = std::ptr::null_mut();
                if GetAce(acl, i, &mut ace) == 0 {
                    return Err(bad());
                }
                let kind = *ace.cast::<u8>();
                if kind == 1 {
                    continue;
                }
                if kind != 0
                    || !allowed.contains(&sid_text(ace.cast::<u8>().add(8).cast())?.as_str())
                {
                    return Err(bad());
                }
            }
            Ok(())
        })();
        LocalFree(descriptor);
        result
    }
}
pub(crate) fn file(path: &Path) -> Result<File, ClientError> {
    no_links(path)?;
    OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(path)
        .map_err(|_| bad())
}
pub(crate) fn directories(path: &Path) -> Result<Vec<File>, ClientError> {
    no_links(path)?;
    let mut pins = Vec::new();
    for p in path.ancestors().skip(1) {
        let name = extended_filesystem_path(p)?;
        unsafe {
            let h = CreateFileW(
                name.as_ptr(),
                0x80,
                3,
                std::ptr::null_mut(),
                3,
                0x02000000,
                std::ptr::null_mut(),
            );
            if h.is_null() || h as isize == -1 {
                return Err(bad());
            }
            pins.push(File::from_raw_handle(h));
        }
    }
    Ok(pins)
}

pub(crate) fn pipe_available(file: &std::process::ChildStdout) -> Result<Option<u32>, ClientError> {
    unsafe {
        let mut available = 0;
        if PeekNamedPipe(
            file.as_raw_handle(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            &mut available,
            std::ptr::null_mut(),
        ) == 0
        {
            if GetLastError() == 109 {
                return Ok(None);
            }
            return Err(error("not_sent", "HOST_UNAVAILABLE"));
        }
        Ok(Some(available))
    }
}

pub(crate) fn peer(stream: &TcpStream, identity: &super::Peer) -> Result<(), ClientError> {
    let fail = || error("not_sent", "HOST_LISTENER_UNTRUSTED");
    unsafe {
        let h = OpenProcess(0x100000 | 0x1000, 0, identity.pid);
        if h.is_null() {
            return Err(fail());
        }
        let result = (|| {
            if WaitForSingleObject(h, 0) != 258 {
                return Err(fail());
            }
            let (mut a, mut b, mut c, mut d) = (
                Time::default(),
                Time::default(),
                Time::default(),
                Time::default(),
            );
            if GetProcessTimes(h, &mut a, &mut b, &mut c, &mut d) == 0
                || ((a.high as u64) << 32 | a.low as u64) != identity.created
            {
                return Err(fail());
            }
            let mut name = vec![0u16; 32768];
            let mut size = name.len() as u32;
            if QueryFullProcessImageNameW(h, 0, name.as_mut_ptr(), &mut size) == 0 {
                return Err(fail());
            }
            let image = String::from_utf16(&name[..size as usize]).map_err(|_| fail())?;
            if image.to_lowercase() != identity.executable.to_lowercase()
                || process_sid(h)? != identity.sid
            {
                return Err(fail());
            }
            let server = stream.peer_addr().map_err(|_| fail())?;
            let client = stream.local_addr().map_err(|_| fail())?;
            if !server.ip().is_loopback() {
                return Err(fail());
            }
            verify_tcp(server.port(), client.port(), identity.pid)
        })();
        CloseHandle(h);
        result
    }
}
#[link(name = "iphlpapi")]
extern "system" {
    fn GetExtendedTcpTable(
        table: H,
        size: *mut u32,
        order: i32,
        family: u32,
        class: u32,
        reserved: u32,
    ) -> u32;
}
unsafe fn verify_tcp(port: u16, remote: u16, pid: u32) -> Result<(), ClientError> {
    let fail = || error("not_sent", "HOST_LISTENER_UNTRUSTED");
    let mut size = 0;
    if GetExtendedTcpTable(std::ptr::null_mut(), &mut size, 0, 2, 5, 0) != 122 {
        return Err(fail());
    }
    for _ in 0..3 {
        if !(4..=16 * 1024 * 1024).contains(&size) {
            return Err(fail());
        }
        let mut raw = vec![0u32; (size as usize + 3) / 4];
        let status = GetExtendedTcpTable(raw.as_mut_ptr().cast(), &mut size, 0, 2, 5, 0);
        if status == 122 {
            continue;
        }
        if status != 0 {
            return Err(fail());
        }
        let bytes = std::slice::from_raw_parts(raw.as_ptr().cast::<u8>(), size as usize);
        let count = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
        if count > 100000 || 4 + count * 24 > bytes.len() {
            return Err(fail());
        }
        let mut matched = false;
        for row in bytes[4..4 + count * 24].chunks_exact(24) {
            if u16::from_be_bytes(row[8..10].try_into().unwrap()) == port
                && u16::from_be_bytes(row[16..18].try_into().unwrap()) == remote
            {
                if row[4..8] != [127, 0, 0, 1]
                    || row[12..16] != [127, 0, 0, 1]
                    || u32::from_le_bytes(row[20..24].try_into().unwrap()) != pid
                {
                    return Err(fail());
                }
                matched = true;
            }
        }
        return if matched { Ok(()) } else { Err(fail()) };
    }
    Err(fail())
}

#[cfg(test)]
mod filesystem_path_tests {
    use super::*;
    use std::os::windows::ffi::OsStringExt;

    fn decoded(path: &Path) -> String {
        let value = extended_filesystem_path(path).unwrap();
        String::from_utf16(&value[..value.len() - 1]).unwrap()
    }

    #[test]
    fn drive_paths_normalize_mixed_separators_and_preserve_unicode() {
        let path = PathBuf::from("C:/Users/Zoë/資料/Portal");
        assert_eq!(decoded(&path), r"\\?\C:\Users\Zoë\資料\Portal");
    }

    #[test]
    fn existing_verbatim_drive_path_is_not_prefixed_twice() {
        let path = PathBuf::from(r"\\?\C:\Users\Zoë\Portal");
        assert_eq!(decoded(&path), r"\\?\C:\Users\Zoë\Portal");
        let mixed = PathBuf::from(r"\\?\C:/Users/Zoë/Portal");
        assert_eq!(decoded(&mixed), r"\\?\C:\Users\Zoë\Portal");
    }

    #[test]
    fn unc_paths_gain_only_the_extended_unc_prefix() {
        let path = PathBuf::from(r"\\server\share\資料");
        assert_eq!(decoded(&path), r"\\?\UNC\server\share\資料");
        let existing = PathBuf::from(r"\\?\UNC\server\share\資料");
        assert_eq!(decoded(&existing), r"\\?\UNC\server\share\資料");
    }

    #[test]
    fn non_unicode_utf16_units_are_preserved() {
        let path = PathBuf::from(std::ffi::OsString::from_wide(&[
            b'C' as u16, b':' as u16, b'\\' as u16, 0xd800, b'\\' as u16, b'x' as u16,
        ]));
        let value = extended_filesystem_path(&path).unwrap();
        assert_eq!(
            &value[..value.len() - 1],
            &[
                b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16,
                b'C' as u16, b':' as u16, b'\\' as u16, 0xd800, b'\\' as u16, b'x' as u16,
            ]
        );
    }

    #[test]
    fn relative_and_unrecognized_device_namespaces_are_refused() {
        assert!(extended_filesystem_path(Path::new("relative/path")).is_err());
        assert!(extended_filesystem_path(Path::new(r"\\.\pipe\name")).is_err());
        assert!(extended_filesystem_path(Path::new(r"\\?\GLOBALROOT\Device")).is_err());
        assert!(extended_filesystem_path(Path::new(r"\\\server\share")).is_err());
    }
}
