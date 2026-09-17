#![deny(unsafe_code)]
use filesystem_tool::preserve_extension;
use std::io;

const PDF: &[u8] = b"%PDF-1.4\nClinch test document\n%%EOF\n";
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n";
const ZIP: &[u8] = b"PK\x05\x06\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0";

#[tokio::test]
async fn detects_pdf_png_and_zip_without_changing_contents() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    for (name, bytes, extension) in [
        ("document", PDF, "pdf"),
        ("image", PNG, "png"),
        ("archive", ZIP, "zip"),
    ] {
        let path = dir.path().join(name);
        tokio::fs::write(&path, bytes).await?;
        let result = preserve_extension(&path).await?;
        assert_eq!(result, path.with_extension(extension));
        assert_eq!(tokio::fs::read(&result).await?, bytes);
        assert!(!path.exists());
        assert_eq!(preserve_extension(&result).await?, result);
    }
    Ok(())
}

#[tokio::test]
async fn preserves_unknown_short_empty_and_already_named_files() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    for (name, bytes) in [
        ("empty", b"".as_slice()),
        ("short", b"%P"),
        ("unknown", b"hello"),
        ("csv", b"name,total\nexample,42\n"),
        ("partial.crdownload", PDF),
        ("existing.custom", PNG),
    ] {
        let path = dir.path().join(name);
        tokio::fs::write(&path, bytes).await?;
        assert_eq!(preserve_extension(&path).await?, path);
        assert_eq!(tokio::fs::read(path).await?, bytes);
    }
    Ok(())
}

#[tokio::test]
async fn refuses_collisions_without_losing_either_file() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let source = dir.path().join("collision");
    let destination = source.with_extension("png");
    tokio::fs::write(&source, PNG).await?;
    tokio::fs::write(&destination, b"original").await?;
    assert!(preserve_extension(&source).await.is_err());
    assert_eq!(tokio::fs::read(destination).await?, b"original");
    assert_eq!(tokio::fs::read(source).await?, PNG);
    Ok(())
}

#[tokio::test]
async fn detects_xlsx_when_identifying_zip_entry_fits_in_sniff_window() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    // ZIP local-file header with an Office spreadsheet entry at offset 30.
    let mut bytes = vec![0; 1024];
    bytes[..4].copy_from_slice(b"PK\x03\x04");
    bytes[30..45].copy_from_slice(b"xl/workbook.xml");
    let path = dir.path().join("spreadsheet");
    tokio::fs::write(&path, &bytes).await?;
    let result = preserve_extension(&path).await?;
    assert_eq!(result, path.with_extension("xlsx"));
    assert_eq!(tokio::fs::read(result).await?, bytes);
    Ok(())
}
