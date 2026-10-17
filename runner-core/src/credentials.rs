//! OS credential-store broker. Secret bytes never enter settings, requests, or logs.

pub fn resolve(reference: &str) -> Option<String> {
    let target = reference.strip_prefix("credential-store:")?;
    if target.is_empty()
        || target.len() > 260
        || !target
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || ".:/_-".contains(c))
    {
        return None;
    }
    #[cfg(windows)]
    {
        use std::slice;
        use windows_sys::Win32::Security::Credentials::{
            CredFree, CredReadW, CREDENTIALW, CRED_TYPE_GENERIC,
        };
        let wide: Vec<u16> = target.encode_utf16().chain(Some(0)).collect();
        let mut credential: *mut CREDENTIALW = std::ptr::null_mut();
        if unsafe { CredReadW(wide.as_ptr(), CRED_TYPE_GENERIC, 0, &mut credential) } == 0
            || credential.is_null()
        {
            return None;
        }
        let entry = unsafe { &*credential };
        let secret = if entry.CredentialBlob.is_null()
            || entry.CredentialBlobSize == 0
            || entry.CredentialBlobSize > 4096
        {
            None
        } else {
            let bytes = unsafe {
                slice::from_raw_parts(entry.CredentialBlob, entry.CredentialBlobSize as usize)
            };
            std::str::from_utf8(bytes)
                .ok()
                .map(str::trim)
                .filter(|s| !s.is_empty() && !s.chars().any(char::is_control))
                .map(str::to_owned)
        };
        unsafe {
            CredFree(credential.cast());
        }
        secret
    }
    #[cfg(not(windows))]
    {
        None
    }
}

pub fn store(target: &str, secret: &str) -> Result<(), String> {
    if target.is_empty()
        || target.len() > 260
        || !target
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || ".:/_-".contains(c))
    {
        return Err("Credential target is invalid.".into());
    }
    let secret = secret.trim();
    if secret.is_empty() || secret.len() > 4096 || secret.chars().any(char::is_control) {
        return Err("Credential value is invalid.".into());
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Security::Credentials::{
            CredWriteW, CREDENTIALW, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC,
        };
        let target_wide: Vec<u16> = target.encode_utf16().chain(Some(0)).collect();
        let mut bytes = secret.as_bytes().to_vec();
        let credential = CREDENTIALW {
            Flags: 0,
            Type: CRED_TYPE_GENERIC,
            TargetName: target_wide.as_ptr() as *mut u16,
            Comment: std::ptr::null_mut(),
            LastWritten: unsafe { std::mem::zeroed() },
            CredentialBlobSize: bytes.len() as u32,
            CredentialBlob: bytes.as_mut_ptr(),
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            AttributeCount: 0,
            Attributes: std::ptr::null_mut(),
            TargetAlias: std::ptr::null_mut(),
            UserName: std::ptr::null_mut(),
        };
        let written = unsafe { CredWriteW(&credential, 0) };
        bytes.fill(0);
        if written == 0 {
            return Err("Windows Credential Manager could not save the credential.".into());
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = (target, secret);
        Err("Credential storage is available only through the supported Windows OS credential store.".into())
    }
}
