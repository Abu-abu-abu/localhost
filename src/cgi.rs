//! cgi.rs — запуск CGI без потоков и без блокирующих pipe.
//!
//! Идея:
//!   * тело запроса пишем во временный файл и отдаём его CGI как stdin
//!     (скрипт видит обычный EOF в конце тела);
//!   * stdout CGI перенаправляем в другой временный файл;
//!   * главный цикл (epoll / WSAPoll) раз в несколько миллисекунд вызывает
//!     `CgiJob::poll()`, внутри которого неблокирующий `try_wait()`.
//!
//! Так нет дедлоков на pipe, нет лишних потоков, есть таймаут, и всё
//! одинаково работает на Windows и на Linux.
//!
//! Подключение: положить файл в `src/cgi.rs` и добавить `mod cgi;` в main.rs.
//! Внешние крейты не нужны.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Максимальный размер вывода CGI (защита памяти сервера).
const MAX_CGI_OUTPUT: u64 = 16 * 1024 * 1024;

static JOB_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Всё, что нужно знать о запросе, чтобы запустить скрипт.
/// Поля заполняются из вашего разобранного HTTP-запроса и конфигурации маршрута.
pub struct CgiRequest<'a> {
    /// Интерпретатор из конфига для расширения файла: "python3" (Linux) или "python" (Windows).
    pub interpreter: &'a str,
    /// Путь к скрипту на диске (root маршрута + остаток URL).
    pub script_path: &'a Path,
    pub method: &'a str,
    /// Исходный URI запроса вместе с query, например "/cgi-bin/hello.py?name=x".
    pub request_uri: &'a str,
    /// Часть после '?', без самого '?'.
    pub query_string: &'a str,
    /// URL-путь скрипта, например "/cgi-bin/hello.py".
    pub script_name: &'a str,
    /// Значение для PATH_INFO. По тексту задания CGI смотрит PATH_INFO, чтобы
    /// узнать полный путь, поэтому сюда передаём полный путь к скрипту.
    pub path_info: &'a str,
    /// Заголовки запроса как (имя, значение).
    pub headers: &'a [(String, String)],
    /// Тело запроса УЖЕ без chunked-обёртки. CONTENT_LENGTH = body.len().
    pub body: &'a [u8],
    pub server_name: &'a str,
    pub server_port: u16,
    pub remote_addr: &'a str,
}

/// Разобранный ответ CGI.
#[derive(Debug, PartialEq)]
pub struct CgiResponse {
    pub status: u16,
    /// Заголовки CGI без Status / Content-Length / Transfer-Encoding / Connection.
    /// Эти четыре сервер выставляет сам.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

pub enum CgiState {
    /// Скрипт ещё работает, вызовите poll() позже.
    Running,
    /// Скрипт закончил, ответ готов.
    Done(CgiResponse),
    /// Ошибка: отдайте клиенту страницу ошибки с этим кодом (500 или 504).
    Failed(u16),
}

pub struct CgiJob {
    child: Child,
    started: Instant,
    timeout: Duration,
    out_path: PathBuf,
    in_path: Option<PathBuf>,
    finished: bool,
}

impl CgiJob {
    /// Запускает скрипт. Не блокирует. Err(код) — какую страницу ошибки отдать.
    pub fn spawn(req: &CgiRequest, timeout: Duration) -> Result<CgiJob, u16> {
        if !req.script_path.is_file() {
            return Err(404);
        }
        let file_name = match req.script_path.file_name() {
            Some(n) => n.to_os_string(),
            None => return Err(500),
        };
        // CGI запускаем из каталога скрипта, чтобы относительные пути работали.
        let dir: PathBuf = match req.script_path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => PathBuf::from("."),
        };

        let id = format!(
            "{}_{}",
            std::process::id(),
            JOB_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let tmp = std::env::temp_dir();
        let out_path = tmp.join(format!("localhost_cgi_{}.out", id));
        let in_path = tmp.join(format!("localhost_cgi_{}.in", id));
        let has_body = !req.body.is_empty();

        let stdin = if has_body {
            match write_input(&in_path, req.body) {
                Ok(f) => Stdio::from(f),
                Err(_) => return Err(500),
            }
        } else {
            Stdio::null()
        };

        let stdout = match create_private_file(&out_path, false) {
            Ok(f) => Stdio::from(f),
            Err(_) => {
                drop(stdin);
                if has_body {
                    let _ = fs::remove_file(&in_path);
                }
                return Err(500);
            }
        };

        let mut cmd = Command::new(req.interpreter);
        // Файл для обработки — первый аргумент (как требует задание).
        cmd.arg(&file_name)
            .current_dir(&dir)
            .stdin(stdin)
            .stdout(stdout)
            .stderr(Stdio::inherit());
        for (k, v) in build_env(req) {
            cmd.env(k, v);
        }

        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[cgi] не удалось запустить '{}': {}", req.interpreter, e);
                drop(cmd);
                let _ = fs::remove_file(&out_path);
                if has_body {
                    let _ = fs::remove_file(&in_path);
                }
                return Err(500);
            }
        };

        Ok(CgiJob {
            child,
            started: Instant::now(),
            timeout,
            out_path,
            in_path: if has_body { Some(in_path) } else { None },
            finished: false,
        })
    }

    /// Неблокирующая проверка. Вызывать из главного цикла после каждого
    /// опроса epoll/WSAPoll, пока у соединения есть активный CgiJob.
    pub fn poll(&mut self) -> CgiState {
        if self.finished {
            return CgiState::Failed(500);
        }
        if fs::metadata(&self.out_path).is_ok_and(|metadata| metadata.len() > MAX_CGI_OUTPUT) {
            self.kill();
            return CgiState::Failed(500);
        }
        match self.child.try_wait() {
            Ok(Some(_status)) => {
                self.finished = true;
                self.collect()
            }
            Ok(None) => {
                if self.started.elapsed() >= self.timeout {
                    self.kill();
                    CgiState::Failed(504)
                } else {
                    CgiState::Running
                }
            }
            Err(_) => {
                self.kill();
                CgiState::Failed(500)
            }
        }
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.finished = true;
    }

    fn collect(&self) -> CgiState {
        match fs::metadata(&self.out_path) {
            Ok(m) if m.len() <= MAX_CGI_OUTPUT => {}
            _ => return CgiState::Failed(500),
        }
        match fs::read(&self.out_path) {
            Ok(raw) if !raw.is_empty() => match parse_output(&raw) {
                Some(resp) => CgiState::Done(resp),
                None => CgiState::Failed(500),
            },
            _ => CgiState::Failed(500),
        }
    }
}

impl Drop for CgiJob {
    /// Если клиент отвалился раньше времени, а CgiJob выбросили, то процесс
    /// убивается, а временные файлы удаляются. Утечек нет.
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        let _ = fs::remove_file(&self.out_path);
        if let Some(p) = &self.in_path {
            let _ = fs::remove_file(p);
        }
    }
}

fn write_input(path: &Path, body: &[u8]) -> std::io::Result<File> {
    let mut f = create_private_file(path, true)?;
    if let Err(error) = f.write_all(body).and_then(|()| f.flush()) {
        drop(f);
        let _ = fs::remove_file(path);
        return Err(error);
    }
    drop(f);
    match File::open(path) {
        Ok(file) => Ok(file),
        Err(error) => {
            let _ = fs::remove_file(path);
            Err(error)
        }
    }
}

fn create_private_file(path: &Path, read: bool) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).read(read).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    options.open(path)
}

fn build_env(req: &CgiRequest) -> Vec<(String, String)> {
    let s = |a: &str, b: &str| (a.to_string(), b.to_string());

    let content_type = req
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
        .map(|(_, v)| v.clone())
        .unwrap_or_default();

    let mut env = vec![
        s("GATEWAY_INTERFACE", "CGI/1.1"),
        s("SERVER_SOFTWARE", "localhost-rs/0.1"),
        s("SERVER_PROTOCOL", "HTTP/1.1"),
        s("SERVER_NAME", req.server_name),
        s("SERVER_PORT", &req.server_port.to_string()),
        s("REQUEST_METHOD", req.method),
        s("REQUEST_URI", req.request_uri),
        s("SCRIPT_NAME", req.script_name),
        s("SCRIPT_FILENAME", &req.script_path.to_string_lossy()),
        s("PATH_INFO", req.path_info),
        s("PATH_TRANSLATED", &req.script_path.to_string_lossy()),
        s("QUERY_STRING", req.query_string),
        s("REMOTE_ADDR", req.remote_addr),
        s("REDIRECT_STATUS", "200"),
        s("CONTENT_LENGTH", &req.body.len().to_string()),
        s("CONTENT_TYPE", &content_type),
    ];

    for (name, value) in req.headers {
        let lname = name.to_ascii_lowercase();
        // "proxy" не пробрасываем (защита от httpoxy).
        if lname == "content-type" || lname == "content-length" || lname == "proxy" {
            continue;
        }
        if name.is_empty() || name.contains('=') || name.contains('\0') || value.contains('\0') {
            continue;
        }
        let key = format!("HTTP_{}", name.to_ascii_uppercase().replace('-', "_"));
        env.push((key, value.clone()));
    }
    env
}

/// Разбор вывода CGI: заголовки, пустая строка, тело.
/// None — вывод не похож на корректный ответ CGI (клиенту уйдёт 500).
pub fn parse_output(raw: &[u8]) -> Option<CgiResponse> {
    let (head_end, body_start) = find_header_end(raw)?;
    let head = std::str::from_utf8(&raw[..head_end]).ok()?;

    let mut status: Option<u16> = None;
    let mut headers: Vec<(String, String)> = Vec::new();

    for line in head.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let (name, value) = line.split_once(':')?;
        let (name, value) = (name.trim(), value.trim());
        if name.is_empty()
            || !name.bytes().all(is_header_token)
            || value.bytes().any(|byte| {
                byte == 0 || byte == b'\r' || byte == b'\n' || (byte < 0x20 && byte != b'\t')
            })
        {
            return None;
        }

        if name.eq_ignore_ascii_case("status") {
            let code: u16 = value.split_whitespace().next()?.parse().ok()?;
            if !(200..=599).contains(&code) {
                return None;
            }
            status = Some(code);
        } else if name.eq_ignore_ascii_case("content-length")
            || name.eq_ignore_ascii_case("transfer-encoding")
            || name.eq_ignore_ascii_case("connection")
        {
            continue;
        } else {
            headers.push((name.to_string(), value.to_string()));
        }
    }

    // По CGI/1.1: Location без Status означает редирект 302.
    let status = status.unwrap_or_else(|| {
        if headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("location"))
        {
            302
        } else {
            200
        }
    });

    Some(CgiResponse {
        status,
        headers,
        body: raw[body_start..].to_vec(),
    })
}

fn is_header_token(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

fn find_header_end(raw: &[u8]) -> Option<(usize, usize)> {
    let crlf = find(raw, b"\r\n\r\n").map(|i| (i, i + 4));
    let lf = find(raw, b"\n\n").map(|i| (i, i + 2));
    match (crlf, lf) {
        (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
        (a, b) => a.or(b),
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn parses_basic_response() {
        let r = parse_output(b"Content-Type: text/plain\r\n\r\nhello").unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body, b"hello");
        assert_eq!(
            r.headers,
            vec![("Content-Type".to_string(), "text/plain".to_string())]
        );
    }

    #[test]
    fn parses_status_header_with_lf_only() {
        let r = parse_output(b"Status: 404 Not Found\nContent-Type: text/html\n\nnope").unwrap();
        assert_eq!(r.status, 404);
        assert_eq!(r.body, b"nope");
        assert_eq!(r.headers.len(), 1);
    }

    #[test]
    fn location_without_status_is_302() {
        let r = parse_output(b"Location: /other\r\n\r\n").unwrap();
        assert_eq!(r.status, 302);
    }

    #[test]
    fn server_owned_headers_are_dropped() {
        let r = parse_output(
            b"Content-Length: 999\r\nTransfer-Encoding: chunked\r\nConnection: close\r\nX-A: 1\r\n\r\nbody",
        )
        .unwrap();
        assert_eq!(r.headers, vec![("X-A".to_string(), "1".to_string())]);
    }

    #[test]
    fn garbage_output_is_rejected() {
        assert!(parse_output(b"no headers here").is_none());
        assert!(parse_output(b"Status: 999\r\n\r\n").is_none());
        assert!(parse_output(b"").is_none());
    }

    #[test]
    fn env_contains_cgi_vars_and_filters_proxy() {
        let headers = vec![
            ("Cookie".to_string(), "a=b".to_string()),
            ("Content-Type".to_string(), "text/plain".to_string()),
            ("Proxy".to_string(), "evil".to_string()),
        ];
        let req = CgiRequest {
            interpreter: "python3",
            script_path: Path::new("www/cgi-bin/hello.py"),
            method: "POST",
            request_uri: "/cgi-bin/hello.py?x=1",
            query_string: "x=1",
            script_name: "/cgi-bin/hello.py",
            path_info: "www/cgi-bin/hello.py",
            headers: &headers,
            body: b"abc",
            server_name: "localhost",
            server_port: 8080,
            remote_addr: "127.0.0.1",
        };
        let env: HashMap<String, String> = build_env(&req).into_iter().collect();
        assert_eq!(env["REQUEST_METHOD"], "POST");
        assert_eq!(env["CONTENT_LENGTH"], "3");
        assert_eq!(env["CONTENT_TYPE"], "text/plain");
        assert_eq!(env["QUERY_STRING"], "x=1");
        assert_eq!(env["PATH_INFO"], "www/cgi-bin/hello.py");
        assert_eq!(env["HTTP_COOKIE"], "a=b");
        assert!(!env.contains_key("HTTP_PROXY"));
    }

    #[test]
    fn missing_script_gives_404() {
        let req = CgiRequest {
            interpreter: "python3",
            script_path: Path::new("definitely/not/here.py"),
            method: "GET",
            request_uri: "/x.py",
            query_string: "",
            script_name: "/x.py",
            path_info: "",
            headers: &[],
            body: b"",
            server_name: "localhost",
            server_port: 8080,
            remote_addr: "127.0.0.1",
        };
        match CgiJob::spawn(&req, Duration::from_secs(1)) {
            Err(code) => assert_eq!(code, 404),
            Ok(_) => panic!("ожидали 404"),
        }
    }
}
