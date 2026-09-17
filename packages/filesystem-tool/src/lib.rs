#![deny(unsafe_code)]
//! Finalize browser GUID downloads without trusting server-provided filenames.
use std::{
    io,
    path::{Path, PathBuf},
};
use tokio::io::AsyncReadExt;

/// Post-process a completed download immediately after CDP confirms write completion.
/// Infer the standard extension from at most the first 512 bytes of extensionless files.
/// Existing filenames and unknown formats are preserved. Never overwrite another file.
/// # Errors
/// Returns file access, publication, or cleanup errors.
pub async fn preserve_extension(path: &Path) -> io::Result<PathBuf> {
    if path.extension().is_some() {
        return Ok(path.to_owned());
    }
    let mut file = tokio::fs::File::open(path).await?;
    let mut header = [0; 512];
    let mut count = 0;
    while count < header.len() {
        let read = file.read(&mut header[count..]).await?;
        if read == 0 {
            break;
        }
        count += read;
    }
    drop(file);
    let Some(kind) = infer::get(&header[..count]) else {
        return Ok(path.to_owned());
    };
    let destination = path.with_extension(kind.extension());
    // Same-directory hard-link publication is atomic and refuses existing destinations.
    tokio::fs::hard_link(path, &destination).await?;
    tokio::fs::remove_file(path).await?;
    Ok(destination)
}
