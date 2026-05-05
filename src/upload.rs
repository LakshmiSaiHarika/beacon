//! Module for uploading zipped folders to the dispatch HTTP server.

use std::fs::File;
use std::io::{Cursor, Read, Seek, Write};
use std::path::Path;

use reqwest::{Body, Client, StatusCode};
use zip::write::FileOptions;
use zip::CompressionMethod;
use zip::ZipWriter;

//use super::*;
//use std::fs;
//use tempfile::tempdir;

/// Recursively zips a folder into an in-memory buffer.
pub fn zip_folder(folder: &Path) -> Result<Vec<u8>, std::io::Error> {
    let mut buffer = Cursor::new(Vec::new());
    {
        let mut zip = ZipWriter::new(&mut buffer);
        let options = FileOptions::<()>::default().compression_method(CompressionMethod::Deflated);

        add_directory_to_zip(&mut zip, folder, folder, options)?;
        zip.finish().map_err(std::io::Error::other)?;
    }

    Ok(buffer.into_inner())
}

/// Recursively adds files from a directory to the zip archive.
fn add_directory_to_zip<W: Write + Seek>(
    zip: &mut ZipWriter<W>,
    base: &Path,
    current: &Path,
    options: FileOptions<()>,
) -> Result<(), std::io::Error> {
    for entry in std::fs::read_dir(current)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = entry.metadata()?;

        let relative = path
            .strip_prefix(base)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;

        let relative_str = relative.to_str().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "path contains invalid UTF-8",
            )
        })?;

        if metadata.is_dir() {
            // Add directory entry (with trailing slash)
            zip.add_directory(format!("{}/", relative_str), options)
                .map_err(std::io::Error::other)?;
            add_directory_to_zip(zip, base, &path, options)?;
        } else if metadata.is_file() {
            zip.start_file(relative_str, options)
                .map_err(std::io::Error::other)?;

            let mut file = File::open(&path)?;
            let mut contents = Vec::new();
            file.read_to_end(&mut contents)?;
            zip.write_all(&contents)?;
        }
    }

    Ok(())
}

/// Uploads a zipped folder to the dispatch server.
pub async fn upload_folder(
    url: &str,
    folder: &Path,
    name: Option<&str>,
) -> Result<bool, Box<dyn std::error::Error>> {
    // Zip the folder synchronously (zip crate is not async)
    let zip_data = tokio::task::spawn_blocking({
        let folder = folder.to_path_buf();
        move || zip_folder(&folder)
    })
    .await??;

    // Determine the archive name
    let archive_name = name
        .map(String::from)
        .or_else(|| {
            folder
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| format!("{}.zip", n))
        })
        .unwrap_or_else(|| "upload.zip".to_string());

    // Upload the zip file
    // Clear "Expect" header to avoid HTTP 417 on servers that don't support "100-continue"
    let response = Client::new()
        .patch(url)
        .header("content-type", "application/zip")
        .header("x-archive-name", &archive_name)
        .header("expect", "")
        .body(Body::from(zip_data))
        .send()
        .await?;

    match response.status() {
        StatusCode::EXPECTATION_FAILED => Ok(false),
        StatusCode::OK | StatusCode::CREATED => Ok(true),
        status => {
            let body = response.text().await.unwrap_or_default();
            eprintln!("warning: {status}: {body}");
            Ok(false)
        }
    }
}
