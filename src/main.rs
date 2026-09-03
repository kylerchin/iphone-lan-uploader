use axum::{
    body::Body,
    extract::{DefaultBodyLimit, Multipart, State},
    http::{header, HeaderMap, HeaderValue, StatusCode, Uri},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Router,
};
use futures_util::stream;
use std::{
    env,
    ffi::OsStr,
    fs as std_fs,
    io::{BufWriter, ErrorKind, Seek, SeekFrom, Write},
    net::UdpSocket,
    path::{Component, Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    net::TcpListener,
};
use tower_http::limit::RequestBodyLimitLayer;
use zip::{write::SimpleFileOptions, CompressionMethod, ZipWriter};

const DOWNLOAD_CHUNK_SIZE: usize = 256 * 1024;
static ARCHIVE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone)]
struct AppState {
    upload_dir: PathBuf,
    archive_dir: PathBuf,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let port: u16 = env::var("PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8787);

    let upload_dir = env::var("UPLOAD_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("iphone_uploads"));
    fs::create_dir_all(&upload_dir).await?;

    let archive_dir = env::temp_dir().join(format!(
        "iphone-lan-uploader-{}-{}",
        std::process::id(),
        unix_nanos()
    ));
    fs::create_dir_all(&archive_dir).await?;

    let max_gb: usize = env::var("MAX_GB")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(100);
    let max_upload_bytes = max_gb * 1024 * 1024 * 1024;

    let state = Arc::new(AppState {
        upload_dir,
        archive_dir,
    });

    let app = Router::new()
        .route("/", get(index))
        .route("/upload", post(upload))
        .route("/files", get(list_upload_folders))
        .route("/files/folder", get(list_folder_files))
        .route("/download/file", get(download_file))
        .route("/download/folder", get(download_folder))
        .with_state(state.clone())
        // Axum's default body limit is small; uploads need an explicit large limit.
        .layer(DefaultBodyLimit::disable())
        .layer(RequestBodyLimitLayer::new(max_upload_bytes));

    let bind_addr = format!("0.0.0.0:{}", port);
    let listener = TcpListener::bind(&bind_addr).await?;

    println!("Saving uploads to: {}", state.upload_dir.display());
    println!("Listening on:       http://{}", bind_addr);
    if let Some(ip) = guess_lan_ip() {
        println!("On the iPhone open: http://{}:{}/", ip, port);
    } else {
        println!("On the iPhone open: http://<your-desktop-LAN-IP>:{}", port);
    }
    println!("Set UPLOAD_DIR, PORT, or MAX_GB env vars to change defaults.");

    axum::serve(listener, app).await?;
    Ok(())
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn upload(
    State(state): State<Arc<AppState>>,
    uri: Uri,
    mut multipart: Multipart,
) -> Result<Html<String>, AppError> {
    let batch_name = query_param(uri.query().unwrap_or(""), "batch")
        .map(sanitize_component)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("batch-{}", unix_seconds()));

    let batch_dir = state.upload_dir.join(batch_name);
    fs::create_dir_all(&batch_dir).await?;

    let mut saved = Vec::new();

    while let Some(mut field) = multipart.next_field().await? {
        let original_name = match field.file_name() {
            Some(name) => name.to_string(),
            None => continue,
        };

        let clean_name = sanitize_filename(&original_name);
        let path = unique_path(&batch_dir, &clean_name).await?;
        let display_name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("uploaded-file")
            .to_string();

        let mut file = fs::File::create(&path).await?;
        let mut bytes_written: u64 = 0;

        while let Some(chunk) = field.chunk().await? {
            bytes_written += chunk.len() as u64;
            file.write_all(&chunk).await?;
        }
        file.flush().await?;

        saved.push((display_name, bytes_written));
    }

    if saved.is_empty() {
        return Err(AppError::bad_request(
            "No file fields were found in the upload.",
        ));
    }

    Ok(Html(upload_result_html(&batch_dir, &saved)))
}

#[derive(Debug)]
struct FolderInfo {
    name: String,
    file_count: usize,
    total_bytes: u64,
    modified: Option<SystemTime>,
}

#[derive(Debug)]
struct FileInfo {
    name: String,
    size: u64,
    modified: Option<SystemTime>,
}

async fn list_upload_folders(State(state): State<Arc<AppState>>) -> Result<Html<String>, AppError> {
    let mut folders = Vec::new();
    let mut entries = fs::read_dir(&state.upload_dir).await?;

    while let Some(entry) = entries.next_entry().await? {
        if !entry.file_type().await?.is_dir() {
            continue;
        }

        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        folders.push(summarize_folder(name, entry.path()).await?);
    }

    folders.sort_by(|left, right| {
        right
            .modified
            .cmp(&left.modified)
            .then_with(|| left.name.cmp(&right.name))
    });

    let mut rows = String::new();
    for folder in &folders {
        let encoded_name = percent_encode(&folder.name);
        rows.push_str(&format!(
            r#"<tr>
  <td class="file-name"><a href="/files/folder?folder={}">{}</a></td>
  <td>{}</td>
  <td>{}</td>
  <td>{}</td>
  <td><a class="download-link" href="/download/folder?folder={}">Download ZIP</a></td>
</tr>"#,
            encoded_name,
            escape_html(&folder.name),
            folder.file_count,
            human_size(folder.total_bytes),
            modified_text(folder.modified),
            encoded_name,
        ));
    }

    if rows.is_empty() {
        rows.push_str(r#"<tr><td colspan="5" class="empty">No upload folders yet.</td></tr>"#);
    }

    let body = format!(
        r#"<h1>Upload folders</h1>
<p>Open a folder to see every uploaded file and its metadata.</p>
<div class="table-wrap">
  <table>
    <thead><tr><th>Folder</th><th>Files</th><th>Size</th><th>Modified</th><th>Download</th></tr></thead>
    <tbody>{}</tbody>
  </table>
</div>"#,
        rows
    );

    Ok(Html(page_html("Upload folders", &body)))
}

async fn summarize_folder(name: String, path: PathBuf) -> Result<FolderInfo, AppError> {
    let mut file_count = 0usize;
    let mut total_bytes = 0u64;
    let mut modified = fs::metadata(&path).await?.modified().ok();
    let mut entries = fs::read_dir(path).await?;

    while let Some(entry) = entries.next_entry().await? {
        if !entry.file_type().await?.is_file() {
            continue;
        }
        let metadata = entry.metadata().await?;
        file_count += 1;
        total_bytes = total_bytes.saturating_add(metadata.len());
        modified = latest_time(modified, metadata.modified().ok());
    }

    Ok(FolderInfo {
        name,
        file_count,
        total_bytes,
        modified,
    })
}

async fn list_folder_files(
    State(state): State<Arc<AppState>>,
    uri: Uri,
) -> Result<Html<String>, AppError> {
    let folder_name = required_query_param(&uri, "folder")?;
    let folder_path = existing_directory(&state.upload_dir, &folder_name).await?;
    let mut files = list_regular_files(&folder_path).await?;
    files.sort_by(|left, right| left.name.cmp(&right.name));

    let encoded_folder = percent_encode(&folder_name);
    let mut rows = String::new();
    for file in &files {
        rows.push_str(&format!(
            r#"<tr>
  <td class="file-name">{}</td>
  <td>{}</td>
  <td>{}</td>
  <td><a class="download-link" href="/download/file?folder={}&amp;file={}">Download</a></td>
</tr>"#,
            escape_html(&file.name),
            human_size(file.size),
            modified_text(file.modified),
            encoded_folder,
            percent_encode(&file.name),
        ));
    }

    if rows.is_empty() {
        rows.push_str(r#"<tr><td colspan="4" class="empty">This folder is empty.</td></tr>"#);
    }

    let body = format!(
        r#"<p class="breadcrumbs"><a href="/files">Upload folders</a> / {}</p>
<h1>{}</h1>
<p><a class="primary-link" href="/download/folder?folder={}">Download entire folder as ZIP</a></p>
<div class="table-wrap">
  <table>
    <thead><tr><th>File</th><th>Size</th><th>Modified</th><th>Download</th></tr></thead>
    <tbody>{}</tbody>
  </table>
</div>"#,
        escape_html(&folder_name),
        escape_html(&folder_name),
        encoded_folder,
        rows,
    );

    Ok(Html(page_html(&folder_name, &body)))
}

async fn list_regular_files(folder: &Path) -> Result<Vec<FileInfo>, AppError> {
    let mut files = Vec::new();
    let mut entries = fs::read_dir(folder).await?;

    while let Some(entry) = entries.next_entry().await? {
        if !entry.file_type().await?.is_file() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let metadata = entry.metadata().await?;
        files.push(FileInfo {
            name,
            size: metadata.len(),
            modified: metadata.modified().ok(),
        });
    }

    Ok(files)
}

async fn download_file(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
) -> Result<Response<Body>, AppError> {
    let folder_name = required_query_param(&uri, "folder")?;
    let file_name = required_query_param(&uri, "file")?;
    let folder_path = existing_directory(&state.upload_dir, &folder_name).await?;
    let file_path = existing_regular_file(&folder_path, &file_name).await?;
    let mut file = fs::File::open(file_path).await?;
    let length = file.metadata().await?.len();

    let requested_range = match headers.get(header::RANGE) {
        Some(value) => match value
            .to_str()
            .ok()
            .and_then(|value| parse_byte_range(value, length))
        {
            Some(range) => Some(range),
            None => return Ok(range_not_satisfiable(length)),
        },
        None => None,
    };

    let (status, start, response_length) = match requested_range {
        Some((start, end)) => {
            file.seek(SeekFrom::Start(start)).await?;
            (StatusCode::PARTIAL_CONTENT, start, end - start + 1)
        }
        None => (StatusCode::OK, 0, length),
    };

    let mut response = Response::new(stream_file(file, response_length, None));
    *response.status_mut() = status;
    set_download_headers(
        &mut response,
        &file_name,
        response_length,
        content_type_for_file(&file_name),
    )?;
    response
        .headers_mut()
        .insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));

    if status == StatusCode::PARTIAL_CONTENT {
        let end = start + response_length - 1;
        insert_header(
            response.headers_mut(),
            header::CONTENT_RANGE,
            &format!("bytes {}-{}/{}", start, end, length),
        )?;
    }

    Ok(response)
}

async fn download_folder(
    State(state): State<Arc<AppState>>,
    uri: Uri,
) -> Result<Response<Body>, AppError> {
    let folder_name = required_query_param(&uri, "folder")?;
    let folder_path = existing_directory(&state.upload_dir, &folder_name).await?;
    let archive_id = ARCHIVE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let archive_path =
        state
            .archive_dir
            .join(format!("folder-{}-{}.zip", unix_nanos(), archive_id));

    let source_path = folder_path.clone();
    let output_path = archive_path.clone();
    let archive_folder_name = folder_name.clone();
    let archive = tokio::task::spawn_blocking(move || {
        let length = build_stored_zip(&source_path, &archive_folder_name, &output_path)?;
        Ok::<_, AppError>(TemporaryArchive::new(output_path, length))
    })
    .await
    .map_err(|error| AppError::internal(format!("Could not build ZIP archive: {}", error)))??;

    let file = fs::File::open(archive.path()).await?;
    let (archive_path, archive_length) = archive.into_parts();

    let download_name = format!("{}.zip", folder_name);
    let mut response = Response::new(stream_file(file, archive_length, Some(archive_path)));
    set_download_headers(
        &mut response,
        &download_name,
        archive_length,
        "application/zip",
    )?;
    Ok(response)
}

async fn existing_directory(root: &Path, name: &str) -> Result<PathBuf, AppError> {
    let path = safe_child(root, name)?;
    match fs::symlink_metadata(&path).await {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(path),
        Ok(_) => Err(AppError::not_found("Upload folder not found.")),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            Err(AppError::not_found("Upload folder not found."))
        }
        Err(error) => Err(error.into()),
    }
}

async fn existing_regular_file(root: &Path, name: &str) -> Result<PathBuf, AppError> {
    let path = safe_child(root, name)?;
    match fs::symlink_metadata(&path).await {
        Ok(metadata) if metadata.file_type().is_file() => Ok(path),
        Ok(_) => Err(AppError::not_found("Uploaded file not found.")),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            Err(AppError::not_found("Uploaded file not found."))
        }
        Err(error) => Err(error.into()),
    }
}

fn safe_child(root: &Path, name: &str) -> Result<PathBuf, AppError> {
    let requested = Path::new(name);
    let mut components = requested.components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(component)), None) if component == OsStr::new(name) => {
            Ok(root.join(component))
        }
        _ => Err(AppError::bad_request("Invalid folder or file name.")),
    }
}

fn required_query_param(uri: &Uri, key: &str) -> Result<String, AppError> {
    query_param(uri.query().unwrap_or(""), key)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AppError::bad_request(format!("Missing {} parameter.", key)))
}

fn parse_byte_range(value: &str, length: u64) -> Option<(u64, u64)> {
    let value = value.strip_prefix("bytes=")?;
    if value.contains(',') || length == 0 {
        return None;
    }

    let (start_text, end_text) = value.split_once('-')?;

    if start_text.is_empty() {
        let suffix_length: u64 = end_text.parse().ok()?;
        if suffix_length == 0 {
            return None;
        }
        let start = length.saturating_sub(suffix_length);
        return Some((start, length - 1));
    }

    let start: u64 = start_text.parse().ok()?;
    if start >= length {
        return None;
    }

    let end = if end_text.is_empty() {
        length - 1
    } else {
        end_text.parse::<u64>().ok()?.min(length - 1)
    };

    if end < start {
        None
    } else {
        Some((start, end))
    }
}

fn range_not_satisfiable(length: u64) -> Response<Body> {
    let mut response = Response::new(Body::from("Requested byte range is not available."));
    *response.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
    if let Ok(value) = HeaderValue::from_str(&format!("bytes */{}", length)) {
        response.headers_mut().insert(header::CONTENT_RANGE, value);
    }
    response
}

struct FileStreamState {
    file: Option<fs::File>,
    remaining: u64,
    cleanup_path: Option<PathBuf>,
}

struct TemporaryArchive {
    path: Option<PathBuf>,
    length: u64,
}

impl TemporaryArchive {
    fn new(path: PathBuf, length: u64) -> Self {
        Self {
            path: Some(path),
            length,
        }
    }

    fn path(&self) -> &Path {
        self.path
            .as_deref()
            .expect("temporary archive path is present")
    }

    fn into_parts(mut self) -> (PathBuf, u64) {
        let path = self.path.take().expect("temporary archive path is present");
        (path, self.length)
    }
}

impl Drop for TemporaryArchive {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = std_fs::remove_file(path);
        }
    }
}

impl Drop for FileStreamState {
    fn drop(&mut self) {
        self.file.take();
        if let Some(path) = self.cleanup_path.take() {
            let _ = std_fs::remove_file(path);
        }
    }
}

fn stream_file(file: fs::File, length: u64, cleanup_path: Option<PathBuf>) -> Body {
    let state = FileStreamState {
        file: Some(file),
        remaining: length,
        cleanup_path,
    };

    let body_stream = stream::try_unfold(state, |mut state| async move {
        if state.remaining == 0 {
            return Ok::<_, std::io::Error>(None);
        }

        let chunk_size = state.remaining.min(DOWNLOAD_CHUNK_SIZE as u64) as usize;
        let mut chunk = vec![0u8; chunk_size];
        let bytes_read = state
            .file
            .as_mut()
            .expect("download stream file is present")
            .read(&mut chunk)
            .await?;

        if bytes_read == 0 {
            return Err(std::io::Error::new(
                ErrorKind::UnexpectedEof,
                "download file changed while it was being sent",
            ));
        }

        chunk.truncate(bytes_read);
        state.remaining -= bytes_read as u64;
        Ok(Some((chunk, state)))
    });

    Body::from_stream(body_stream)
}

fn set_download_headers(
    response: &mut Response<Body>,
    file_name: &str,
    length: u64,
    content_type: &'static str,
) -> Result<(), AppError> {
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    insert_header(
        response.headers_mut(),
        header::CONTENT_LENGTH,
        &length.to_string(),
    )?;
    insert_header(
        response.headers_mut(),
        header::CONTENT_DISPOSITION,
        &format!(
            "attachment; filename=\"{}\"",
            legacy_header_filename(file_name)
        ),
    )?;
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    Ok(())
}

fn insert_header(
    headers: &mut HeaderMap,
    name: header::HeaderName,
    value: &str,
) -> Result<(), AppError> {
    let value = HeaderValue::from_str(value)
        .map_err(|_| AppError::internal("Could not construct download headers."))?;
    headers.insert(name, value);
    Ok(())
}

fn legacy_header_filename(name: &str) -> String {
    let mut safe = String::new();
    for character in name.chars() {
        if character.is_ascii_alphanumeric()
            || matches!(character, ' ' | '.' | '-' | '_' | '(' | ')')
        {
            safe.push(character);
        } else {
            safe.push('_');
        }
    }

    let safe = safe.trim_matches(|character| character == ' ' || character == '.');
    if safe.is_empty() {
        "download".to_string()
    } else {
        safe.chars().take(180).collect()
    }
}

fn content_type_for_file(name: &str) -> &'static str {
    let extension = Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    match extension.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "heic" | "heif" => "image/heic",
        "tif" | "tiff" => "image/tiff",
        "mov" => "video/quicktime",
        "mp4" | "m4v" => "video/mp4",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "wav" => "audio/wav",
        "pdf" => "application/pdf",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn build_stored_zip(folder: &Path, folder_name: &str, output: &Path) -> Result<u64, AppError> {
    let result = build_stored_zip_inner(folder, folder_name, output);
    if result.is_err() {
        let _ = std_fs::remove_file(output);
    }
    result
}

fn build_stored_zip_inner(
    folder: &Path,
    folder_name: &str,
    output: &Path,
) -> Result<u64, AppError> {
    let mut source_files = Vec::new();
    for entry in std_fs::read_dir(folder)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        source_files.push((name, entry.path()));
    }
    source_files.sort_by(|left, right| left.0.cmp(&right.0));

    let output_file = std_fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)?;
    let mut archive = ZipWriter::new(BufWriter::new(output_file));
    let archive_folder = zip_path_component(folder_name);
    let directory_options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Stored)
        .unix_permissions(0o755);
    archive.add_directory(format!("{}/", archive_folder), directory_options)?;

    for (file_name, path) in source_files {
        let mut source = std_fs::File::open(path)?;
        let size = source.metadata()?.len();
        let archive_name = format!("{}/{}", archive_folder, zip_path_component(&file_name));
        let file_options = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Stored)
            .large_file(size >= u32::MAX as u64)
            .unix_permissions(0o644);
        archive.start_file(archive_name, file_options)?;
        std::io::copy(&mut source, &mut archive)?;
    }

    let mut output = archive.finish()?;
    output.flush()?;
    Ok(output.stream_position()?)
}

fn zip_path_component(name: &str) -> String {
    let mut safe = String::with_capacity(name.len());
    for character in name.chars() {
        if character == '/' || character == '\\' || character == '\0' {
            safe.push('_');
        } else {
            safe.push(character);
        }
    }
    if safe.is_empty() {
        "upload".to_string()
    } else {
        safe
    }
}
fn latest_time(left: Option<SystemTime>, right: Option<SystemTime>) -> Option<SystemTime> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

fn modified_text(modified: Option<SystemTime>) -> String {
    modified
        .map(httpdate::fmt_http_date)
        .unwrap_or_else(|| "Unknown".to_string())
}

fn percent_encode(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[(byte >> 4) as usize]));
            encoded.push(char::from(HEX[(byte & 0x0f) as usize]));
        }
    }
    encoded
}

fn page_html(title: &str, body: &str) -> String {
    format!(
        r#"<!doctype html>
<html>
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>{}</title>
  <style>{}</style>
</head>
<body>
  {}
  <main>{}</main>
</body>
</html>"#,
        escape_html(title),
        PAGE_STYLE,
        SITE_HEADER,
        body,
    )
}

const SITE_HEADER: &str = r#"<header class="site-header">
  <a class="brand" href="/">iPhone LAN Uploader</a>
  <a class="nav-link" href="/files">Browse files</a>
</header>"#;

const PAGE_STYLE: &str = r#"
* { -webkit-box-sizing: border-box; box-sizing: border-box; }
body {
  font-family: -apple-system, Helvetica, Arial, sans-serif;
  margin: 0;
  line-height: 1.35;
  background: #fff;
  color: #111;
}
.site-header {
  padding: 12px 14px;
  border-bottom: 1px solid #bbb;
  background: #f7f7f7;
}
.site-header a { display: inline-block; padding: 6px 8px; }
.brand { color: #111; font-size: 18px; font-weight: bold; text-decoration: none; }
.nav-link { margin-left: 8px; font-size: 16px; }
main { padding: 16px; }
h1 { font-size: 24px; margin: 0 0 12px 0; }
p { margin: 10px 0; }
.table-wrap { width: 100%; overflow-x: auto; -webkit-overflow-scrolling: touch; }
table { width: 100%; border-collapse: collapse; font-size: 14px; }
th, td { padding: 9px 8px; border-bottom: 1px solid #ccc; text-align: left; vertical-align: top; }
th { background: #f2f2f2; }
.file-name { min-width: 140px; word-break: break-all; word-wrap: break-word; }
.download-link, .primary-link { display: inline-block; min-height: 34px; padding: 7px 9px; }
.primary-link { border: 1px solid #777; border-radius: 5px; background: #f7f7f7; }
.breadcrumbs { font-size: 14px; color: #444; }
.empty { padding: 18px 8px; color: #555; }
@media screen and (max-width: 360px) {
  main { padding: 12px 8px; }
  .site-header { padding: 8px 4px; }
  .site-header a { padding: 7px 5px; }
  .nav-link { margin-left: 2px; }
  th, td { padding: 8px 6px; }
}
"#;

fn upload_result_html(batch_dir: &Path, saved: &[(String, u64)]) -> String {
    let mut rows = String::new();
    for (name, size) in saved {
        rows.push_str(&format!(
            "<li><code>{}</code> - {}</li>",
            escape_html(name),
            human_size(*size)
        ));
    }

    let folder_name = batch_dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    let folder_link = if folder_name.is_empty() {
        "/files".to_string()
    } else {
        format!("/files/folder?folder={}", percent_encode(folder_name))
    };
    let body = format!(
        r#"<h1>Upload complete</h1>
<p>Saved to desktop folder:</p>
<p><code>{}</code></p>
<ul>{}</ul>
<p><a class="primary-link" href="{}">View uploaded files</a></p>
<p><a href="/">Upload more</a></p>"#,
        escape_html(&batch_dir.display().to_string()),
        rows,
        folder_link,
    );
    page_html("Upload complete", &body)
}

fn query_param(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        let mut parts = pair.splitn(2, '=');
        let k = parts.next().unwrap_or("");
        let v = parts.next().unwrap_or("");
        if k == key {
            return Some(percent_decode(v));
        }
    }
    None
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                if let (Some(hi), Some(lo)) = (from_hex(bytes[i + 1]), from_hex(bytes[i + 2])) {
                    out.push((hi << 4) | lo);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn from_hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn sanitize_component(s: String) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(80)
        .collect()
}

fn sanitize_filename(name: &str) -> String {
    let base = Path::new(name)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("upload.bin");

    let mut out = String::new();
    for ch in base.chars() {
        if ch.is_ascii_alphanumeric() || ch == '.' || ch == '-' || ch == '_' || ch == ' ' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }

    let trimmed = out.trim_matches(|c| c == ' ' || c == '.').to_string();
    if trimmed.is_empty() {
        "upload.bin".to_string()
    } else {
        trimmed.chars().take(180).collect()
    }
}

async fn unique_path(dir: &Path, name: &str) -> Result<PathBuf, AppError> {
    let candidate = dir.join(name);
    if fs::metadata(&candidate).await.is_err() {
        return Ok(candidate);
    }

    let path = Path::new(name);
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("upload");
    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");

    for n in 1..10_000u32 {
        let file_name = if ext.is_empty() {
            format!("{}-{}", stem, n)
        } else {
            format!("{}-{}.{}", stem, n, ext)
        };
        let candidate = dir.join(file_name);
        if fs::metadata(&candidate).await.is_err() {
            return Ok(candidate);
        }
    }

    Err(AppError::internal(
        "Could not find a unique output filename.",
    ))
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn unix_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn guess_lan_ip() -> Option<std::net::IpAddr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    Some(socket.local_addr().ok()?.ip())
}

fn human_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.2} GB", b / GB)
    } else if b >= MB {
        format!("{:.2} MB", b / MB)
    } else if b >= KB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{} B", bytes)
    }
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[derive(Debug)]
struct AppError {
    status: StatusCode,
    message: String,
}

impl AppError {
    fn bad_request(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: msg.into(),
        }
    }

    fn not_found(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: msg.into(),
        }
    }

    fn internal(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: msg.into(),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response<Body> {
        let body = format!(
            r#"<h1>Request error</h1><p>{}</p><p><a href="/">Return to upload</a></p>"#,
            escape_html(&self.message)
        );
        let html = page_html("Request error", &body);
        (
            self.status,
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            html,
        )
            .into_response()
    }
}

impl From<std::io::Error> for AppError {
    fn from(err: std::io::Error) -> Self {
        Self::internal(err.to_string())
    }
}

impl From<zip::result::ZipError> for AppError {
    fn from(err: zip::result::ZipError) -> Self {
        Self::internal(err.to_string())
    }
}

impl From<axum::extract::multipart::MultipartError> for AppError {
    fn from(err: axum::extract::multipart::MultipartError) -> Self {
        Self::bad_request(err.to_string())
    }
}

const INDEX_HTML: &str = r#"<!doctype html>
<html>
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>iPhone LAN Upload</title>
  <style>
    * { -webkit-box-sizing: border-box; box-sizing: border-box; }
    body {
      font-family: -apple-system, Helvetica, Arial, sans-serif;
      margin: 0;
      line-height: 1.35;
      background: #fff;
      color: #111;
    }
    .site-header {
      padding: 12px 14px;
      border-bottom: 1px solid #bbb;
      background: #f7f7f7;
    }
    .site-header a { display: inline-block; padding: 6px 8px; }
    .brand { color: #111; font-size: 18px; font-weight: bold; text-decoration: none; }
    .nav-link { margin-left: 8px; font-size: 16px; }
    main { padding: 18px; }
    h1 { font-size: 24px; margin: 0 0 12px 0; }
    p { margin: 10px 0; }
    .box {
      border: 1px solid #bbb;
      border-radius: 8px;
      padding: 14px;
      margin: 12px 0;
      background: #f7f7f7;
    }
    input, button {
      font-size: 18px;
      max-width: 100%;
    }
    button {
      margin-top: 12px;
      padding: 10px 14px;
    }
    #progressOuter {
      display: none;
      border: 1px solid #999;
      height: 24px;
      margin-top: 12px;
      width: 100%;
    }
    #progressInner {
      height: 24px;
      width: 0%;
      background: #777;
    }
    #status, #current {
      word-break: break-word;
    }
    .small { font-size: 14px; color: #444; }
    @media screen and (max-width: 360px) {
      main { padding: 14px 10px; }
      .site-header { padding: 8px 4px; }
      .site-header a { padding: 7px 5px; }
      .nav-link { margin-left: 2px; }
    }
  </style>
</head>
<body>
  <header class="site-header">
    <a class="brand" href="/">iPhone LAN Uploader</a>
    <a class="nav-link" href="/files">Browse files</a>
  </header>
  <main>
  <h1>iPhone LAN Upload</h1>
  <p>Select photos or videos, then upload them directly to this computer.</p>

  <div class="box">
    <form id="uploadForm" action="/upload" method="post" enctype="multipart/form-data">
      <p>
        <input id="files" name="files" type="file" accept="image/*,video/*" multiple>
      </p>
      <button id="send" type="submit">Upload selected files</button>
    </form>

    <div id="progressOuter"><div id="progressInner"></div></div>
    <p id="status" class="small">Ready.</p>
    <p id="current" class="small"></p>
  </div>

  <p class="small">
    Compatibility note: this page uses an ordinary HTML file upload form. If your Safari supports old-style
    XMLHttpRequest uploads, it will upload files one at a time with progress. If not, the normal form submit is used.
  </p>
  <p class="small">
    For a large camera roll, upload in batches. Videos can take a while over Wi-Fi; keep Safari open until it finishes.
  </p>
  </main>

<script type="text/javascript">
(function () {
  // Written as ES5-era JavaScript for old Mobile Safari: no fetch, Promise,
  // async/await, let/const, arrow functions, modules, classList, or template strings.

  var form = document.getElementById('uploadForm');
  var input = document.getElementById('files');
  var button = document.getElementById('send');
  var progressOuter = document.getElementById('progressOuter');
  var progressInner = document.getElementById('progressInner');

  function setText(id, text) {
    var el = document.getElementById(id);
    if (!el) { return; }
    if (typeof el.textContent !== 'undefined') {
      el.textContent = text;
    } else {
      el.innerText = text;
    }
  }

  function canUseAjaxUpload() {
    return !!(window.XMLHttpRequest && window.FormData && input && input.files);
  }

  function makeBatchId() {
    return 'batch-' + String(new Date().getTime()) + '-' + String(Math.floor(Math.random() * 100000));
  }

  function fileSizeText(size) {
    if (typeof size !== 'number') { return ''; }
    if (size >= 1024 * 1024 * 1024) { return (size / (1024 * 1024 * 1024)).toFixed(2) + ' GB'; }
    if (size >= 1024 * 1024) { return (size / (1024 * 1024)).toFixed(2) + ' MB'; }
    if (size >= 1024) { return (size / 1024).toFixed(1) + ' KB'; }
    return String(size) + ' B';
  }

  function setProgress(percent) {
    if (percent < 0) { percent = 0; }
    if (percent > 100) { percent = 100; }
    progressOuter.style.display = 'block';
    progressInner.style.width = String(percent) + '%';
  }

  function uploadOne(files, index, batchId, doneCount) {
    var file = files[index];
    var fd = new FormData();
    var xhr = new XMLHttpRequest();
    var displayName = file.name || ('file-' + String(index + 1));

    try {
      fd.append('file', file, displayName);
    } catch (e) {
      fd.append('file', file);
    }

    setText('status', 'Uploading ' + String(index + 1) + ' of ' + String(files.length) + '...');
    setText('current', displayName + ' ' + fileSizeText(file.size));
    setProgress(0);

    if (xhr.upload) {
      xhr.upload.onprogress = function (evt) {
        if (evt && evt.lengthComputable) {
          setProgress(Math.round((evt.loaded / evt.total) * 100));
        }
      };
    }

    xhr.onreadystatechange = function () {
      if (xhr.readyState !== 4) { return; }
      if (xhr.status >= 200 && xhr.status < 300) {
        setProgress(100);
        if (index + 1 < files.length) {
          uploadOne(files, index + 1, batchId, doneCount + 1);
        } else {
          button.disabled = false;
          setText('status', 'Done. Uploaded ' + String(files.length) + ' file(s).');
          setText('current', 'Saved into folder ' + batchId + ' on the desktop computer.');
        }
      } else {
        button.disabled = false;
        setText('status', 'Upload failed with HTTP ' + String(xhr.status) + '. Try a smaller batch or use the normal form submit by disabling JavaScript.');
      }
    };

    xhr.onerror = function () {
      button.disabled = false;
      setText('status', 'Upload failed. Check Wi-Fi, desktop firewall, and that the server is still running.');
    };

    xhr.open('POST', '/upload?batch=' + encodeURIComponent(batchId), true);
    xhr.send(fd);
  }

  form.onsubmit = function (evt) {
    var files;

    if (!canUseAjaxUpload()) {
      return true; // fall back to regular multipart form POST
    }

    files = input.files;
    if (!files || files.length < 1) {
      setText('status', 'Choose at least one photo or video first.');
      return false;
    }

    if (evt && evt.preventDefault) { evt.preventDefault(); }
    if (window.event) { window.event.returnValue = false; }

    button.disabled = true;
    progressInner.style.width = '0%';
    uploadOne(files, 0, makeBatchId(), 0);
    return false;
  };
}());
</script>
</body>
</html>
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_ranges_support_fixed_open_and_suffix_forms() {
        assert_eq!(parse_byte_range("bytes=0-4", 10), Some((0, 4)));
        assert_eq!(parse_byte_range("bytes=5-", 10), Some((5, 9)));
        assert_eq!(parse_byte_range("bytes=-3", 10), Some((7, 9)));
        assert_eq!(parse_byte_range("bytes=-30", 10), Some((0, 9)));
    }

    #[test]
    fn byte_ranges_reject_invalid_or_multiple_ranges() {
        assert_eq!(parse_byte_range("bytes=10-12", 10), None);
        assert_eq!(parse_byte_range("bytes=8-4", 10), None);
        assert_eq!(parse_byte_range("bytes=0-1,4-5", 10), None);
        assert_eq!(parse_byte_range("items=0-1", 10), None);
        assert_eq!(parse_byte_range("bytes=0-1", 0), None);
    }

    #[test]
    fn safe_child_rejects_path_traversal() {
        let root = Path::new("uploads");
        assert_eq!(safe_child(root, "batch-1").unwrap(), root.join("batch-1"));
        assert!(safe_child(root, "../private").is_err());
        assert!(safe_child(root, "/private").is_err());
        assert!(safe_child(root, ".").is_err());
    }

    #[test]
    fn query_values_round_trip_through_percent_encoding() {
        let original = "photo one (1).jpg";
        assert_eq!(percent_decode(&percent_encode(original)), original);
    }
}
