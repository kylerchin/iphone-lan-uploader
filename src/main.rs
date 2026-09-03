use axum::{
    body::Body,
    extract::{DefaultBodyLimit, Multipart, State},
    http::{header, StatusCode, Uri},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Router,
};
use std::{
    env,
    net::UdpSocket,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{fs, io::AsyncWriteExt, net::TcpListener};
use tower_http::limit::RequestBodyLimitLayer;

#[derive(Clone)]
struct AppState {
    upload_dir: PathBuf,
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

    let max_gb: usize = env::var("MAX_GB")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(100);
    let max_upload_bytes = max_gb * 1024 * 1024 * 1024;

    let state = Arc::new(AppState { upload_dir });

    let app = Router::new()
        .route("/", get(index))
        .route("/upload", post(upload))
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
        return Err(AppError::bad_request("No file fields were found in the upload."));
    }

    Ok(Html(upload_result_html(&batch_dir, &saved)))
}

fn upload_result_html(batch_dir: &Path, saved: &[(String, u64)]) -> String {
    let mut rows = String::new();
    for (name, size) in saved {
        rows.push_str(&format!(
            "<li><code>{}</code> - {}</li>",
            escape_html(name),
            human_size(*size)
        ));
    }

    format!(
        r#"<!doctype html>
<html>
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Upload complete</title>
  <style>
    body {{ font-family: -apple-system, Helvetica, Arial, sans-serif; margin: 20px; line-height: 1.35; }}
    code {{ word-break: break-all; }}
    a, button {{ font-size: 18px; }}
  </style>
</head>
<body>
  <h1>Upload complete</h1>
  <p>Saved to desktop folder:</p>
  <p><code>{}</code></p>
  <ul>{}</ul>
  <p><a href="/">Upload more</a></p>
</body>
</html>"#,
        escape_html(&batch_dir.display().to_string()),
        rows
    )
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
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("upload");
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

    Err(AppError::internal("Could not find a unique output filename."))
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
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
        Self { status: StatusCode::BAD_REQUEST, message: msg.into() }
    }

    fn internal(msg: impl Into<String>) -> Self {
        Self { status: StatusCode::INTERNAL_SERVER_ERROR, message: msg.into() }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response<Body> {
        let html = format!(
            r#"<!doctype html><html><head><meta charset="utf-8"><title>Upload error</title></head>
<body><h1>Upload error</h1><p>{}</p><p><a href="/">Try again</a></p></body></html>"#,
            escape_html(&self.message)
        );
        (self.status, [(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response()
    }
}

impl From<std::io::Error> for AppError {
    fn from(err: std::io::Error) -> Self {
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
    body {
      font-family: -apple-system, Helvetica, Arial, sans-serif;
      margin: 18px;
      line-height: 1.35;
      background: #fff;
      color: #111;
    }
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
  </style>
</head>
<body>
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
