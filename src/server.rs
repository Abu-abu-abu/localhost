use crate::cgi::{CgiJob, CgiRequest, CgiResponse, CgiState};
use crate::config::{Config, Route, ServerConfig};
use crate::http::{parse_request, HttpError, Request, MAX_HEADER_BYTES};
use crate::status_reason;
use std::collections::HashMap;
use std::fs;
use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL_MS: i32 = 250;
const CGI_POLL_INTERVAL_MS: i32 = 10;
const MAX_CLIENTS: usize = 4096;
const MAX_REQUESTS_PER_CONNECTION: usize = 100;
const MAX_STATIC_FILE_BYTES: u64 = 64 * 1024 * 1024;
const READABLE: u32 = 1;
const WRITABLE: u32 = 2;

struct ListenerGroup {
    listener: TcpListener,
    server_indices: Vec<usize>,
    token: u64,
}

struct Client {
    stream: TcpStream,
    listener_group: usize,
    server_index: usize,
    peer_addr: String,
    input: Vec<u8>,
    response: Vec<u8>,
    response_offset: usize,
    writing: bool,
    cgi: Option<CgiJob>,
    cgi_set_cookie: Option<String>,
    keep_alive: bool,
    close_after_write: bool,
    request_bytes: usize,
    requests_served: usize,
    started_at: Instant,
    last_activity: Instant,
}

struct Reply {
    status: u16,
    body: Vec<u8>,
    content_type: String,
    extra_headers: Vec<(String, String)>,
}

pub fn run(config: Config) -> io::Result<()> {
    let mut endpoint_groups: Vec<(SocketAddr, Vec<usize>)> = Vec::new();
    for (index, server) in config.servers.iter().enumerate() {
        let ip = server.address.parse::<IpAddr>().map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid endpoint address: {error}"),
            )
        })?;
        let address = SocketAddr::new(ip, server.port);
        if let Some((_, indices)) = endpoint_groups
            .iter_mut()
            .find(|(endpoint, _)| *endpoint == address)
        {
            indices.push(index);
        } else {
            endpoint_groups.push((address, vec![index]));
        }
    }

    let mut poller = Poller::new()?;
    let mut listeners = Vec::new();
    let mut listener_tokens = HashMap::new();
    let mut next_token = 1u64;
    for (address, server_indices) in endpoint_groups {
        let listener = TcpListener::bind(address)?;
        listener.set_nonblocking(true)?;
        let token = next_token;
        next_token += 1;
        poller.add(token, raw_socket_listener(&listener), READABLE)?;
        listener_tokens.insert(token, listeners.len());
        listeners.push(ListenerGroup {
            listener,
            server_indices,
            token,
        });
    }
    let mut clients = HashMap::<u64, Client>::new();
    let mut sessions = HashMap::<String, Instant>::new();
    for group in &listeners {
        let address = group.listener.local_addr()?;
        let names = group
            .server_indices
            .iter()
            .filter_map(|index| config.servers.get(*index))
            .map(|server| server.server_name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        println!("LocalHost listening on {address} (hosts: {names})");
    }

    loop {
        let has_cgi = clients.values().any(|client| client.cgi.is_some());
        let wait_ms = if has_cgi {
            CGI_POLL_INTERVAL_MS
        } else {
            POLL_INTERVAL_MS
        };
        let ready_events = poller.wait(wait_ms)?;
        for event in ready_events {
            if let Some(&group_index) = listener_tokens.get(&event.token) {
                accept_clients(
                    &listeners[group_index],
                    group_index,
                    &mut clients,
                    &mut poller,
                    &mut next_token,
                )?;
                continue;
            }

            let Some(client) = clients.get_mut(&event.token) else {
                continue;
            };
            let mut close_client = event.error;
            let mut interest = READABLE;
            if !close_client && client.cgi.is_some() && event.readable {
                let mut probe = [0u8; 1];
                match client.stream.peek(&mut probe) {
                    Ok(0) => close_client = true,
                    Ok(_) => close_client = true,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => close_client = true,
                }
                interest = 0;
            } else if !close_client && !client.writing && client.cgi.is_none() && event.readable {
                match read_request(client, &config, &listeners, &mut sessions) {
                    Ok(ReadState::Waiting) => {}
                    Ok(ReadState::Ready) => {
                        client.writing = true;
                        interest = WRITABLE;
                    }
                    Ok(ReadState::WaitingCgi) => interest = 0,
                    Err(error) => {
                        let reply = error_reply(
                            &config.servers[client.server_index],
                            status_for_io(&error),
                            "Request failed",
                        );
                        client.keep_alive = false;
                        client.close_after_write = true;
                        client.response = serialize_reply(reply, None, false);
                        client.response_offset = 0;
                        client.writing = true;
                        interest = WRITABLE;
                    }
                }
            }
            if !close_client && client.writing && event.writable {
                match write_response(client) {
                    Ok(true) => {
                        if client.keep_alive
                            && !client.close_after_write
                            && client.requests_served < MAX_REQUESTS_PER_CONNECTION
                        {
                            client
                                .input
                                .drain(..client.request_bytes.min(client.input.len()));
                            client.response.clear();
                            client.response_offset = 0;
                            client.writing = false;
                            client.cgi_set_cookie = None;
                            client.keep_alive = false;
                            client.close_after_write = false;
                            client.request_bytes = 0;
                            client.started_at = Instant::now();
                            client.last_activity = client.started_at;
                            match read_request(client, &config, &listeners, &mut sessions) {
                                Ok(ReadState::Waiting) => interest = READABLE,
                                Ok(ReadState::Ready) => {
                                    client.writing = true;
                                    interest = WRITABLE;
                                }
                                Ok(ReadState::WaitingCgi) => interest = 0,
                                Err(_) => close_client = true,
                            }
                        } else {
                            close_client = true;
                        }
                    }
                    Ok(false) => interest = WRITABLE,
                    Err(_) => close_client = true,
                }
            }
            client.last_activity = Instant::now();
            if close_client {
                if let Some(client) = clients.remove(&event.token) {
                    poller.delete(event.token, raw_socket_stream(&client.stream));
                }
            } else {
                poller.modify(event.token, raw_socket_stream(&client.stream), interest)?;
            }
        }

        let mut cgi_ready = Vec::new();
        for (&token, client) in clients.iter_mut() {
            let state = client.cgi.as_mut().map(CgiJob::poll);
            match state {
                Some(CgiState::Running) | None => {}
                Some(CgiState::Done(response)) => {
                    client.cgi = None;
                    let reply = cgi_reply(response);
                    client.response =
                        serialize_reply(reply, client.cgi_set_cookie.as_deref(), client.keep_alive);
                    client.cgi_set_cookie = None;
                    client.response_offset = 0;
                    client.writing = true;
                    cgi_ready.push(token);
                }
                Some(CgiState::Failed(code)) => {
                    client.cgi = None;
                    let reply = error_reply(
                        &config.servers[client.server_index],
                        code,
                        "CGI execution failed",
                    );
                    client.response =
                        serialize_reply(reply, client.cgi_set_cookie.as_deref(), client.keep_alive);
                    client.cgi_set_cookie = None;
                    client.response_offset = 0;
                    client.writing = true;
                    cgi_ready.push(token);
                }
            }
        }
        for token in cgi_ready {
            if let Some(client) = clients.get(&token) {
                poller.modify(token, raw_socket_stream(&client.stream), WRITABLE)?;
            }
        }

        let now = Instant::now();
        let expired: Vec<u64> = clients
            .iter()
            .filter_map(|(token, client)| {
                (now.duration_since(client.started_at) > REQUEST_TIMEOUT
                    || now.duration_since(client.last_activity) > REQUEST_TIMEOUT)
                    .then_some(*token)
            })
            .collect();
        for token in expired {
            if let Some(client) = clients.remove(&token) {
                poller.delete(token, raw_socket_stream(&client.stream));
            }
        }
        sessions.retain(|_, created| now.duration_since(*created) < Duration::from_secs(60 * 60));
    }
}

#[derive(Debug)]
enum ReadState {
    Waiting,
    Ready,
    WaitingCgi,
}

fn accept_clients(
    group: &ListenerGroup,
    group_index: usize,
    clients: &mut HashMap<u64, Client>,
    poller: &mut Poller,
    next_token: &mut u64,
) -> io::Result<()> {
    loop {
        match group.listener.accept() {
            Ok((stream, peer)) => {
                if clients.len() >= MAX_CLIENTS {
                    drop(stream);
                    continue;
                }
                stream.set_nonblocking(true)?;
                let token = *next_token;
                *next_token += 1;
                poller.add(token, raw_socket_stream(&stream), READABLE)?;
                clients.insert(
                    token,
                    Client {
                        stream,
                        listener_group: group_index,
                        server_index: group.server_indices[0],
                        peer_addr: peer.ip().to_string(),
                        input: Vec::new(),
                        response: Vec::new(),
                        response_offset: 0,
                        writing: false,
                        cgi: None,
                        cgi_set_cookie: None,
                        keep_alive: false,
                        close_after_write: false,
                        request_bytes: 0,
                        requests_served: 0,
                        started_at: Instant::now(),
                        last_activity: Instant::now(),
                    },
                );
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    let _ = group.token;
    Ok(())
}

fn read_request(
    client: &mut Client,
    config: &Config,
    listeners: &[ListenerGroup],
    sessions: &mut HashMap<String, Instant>,
) -> io::Result<ReadState> {
    let server_indices = &listeners[client.listener_group].server_indices;
    let body_limit = server_indices
        .iter()
        .filter_map(|index| config.servers.get(*index))
        .map(|server| server.max_body)
        .max()
        .unwrap_or(1024 * 1024);
    let mut temporary = [0u8; 8192];
    loop {
        if client.input.len()
            > body_limit
                .saturating_add(MAX_HEADER_BYTES)
                .saturating_add(64)
        {
            let default_server = &config.servers[server_indices[0]];
            client.response = serialize_reply(
                error_reply(default_server, 413, "Request body is too large"),
                None,
                false,
            );
            client.response_offset = 0;
            client.keep_alive = false;
            client.close_after_write = true;
            client.request_bytes = client.input.len();
            return Ok(ReadState::Ready);
        }
        match parse_request(&client.input, body_limit) {
            Ok(Some(request)) => {
                return prepare_request(client, config, server_indices, request, sessions);
            }
            Ok(None) => {}
            Err(HttpError::BadRequest) => {
                let default_server = &config.servers[server_indices[0]];
                client.response = serialize_reply(
                    error_reply(default_server, 400, "Malformed HTTP request"),
                    None,
                    false,
                );
                client.response_offset = 0;
                client.keep_alive = false;
                client.close_after_write = true;
                client.request_bytes = client.input.len();
                return Ok(ReadState::Ready);
            }
            Err(HttpError::PayloadTooLarge) => {
                let default_server = &config.servers[server_indices[0]];
                client.response = serialize_reply(
                    error_reply(default_server, 413, "Request body is too large"),
                    None,
                    false,
                );
                client.response_offset = 0;
                client.keep_alive = false;
                client.close_after_write = true;
                client.request_bytes = client.input.len();
                return Ok(ReadState::Ready);
            }
        }
        match client.stream.read(&mut temporary) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "client closed request",
                ))
            }
            Ok(read) => client.input.extend_from_slice(&temporary[..read]),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                return Ok(ReadState::Waiting)
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}

fn prepare_request(
    client: &mut Client,
    config: &Config,
    server_indices: &[usize],
    request: Request,
    sessions: &mut HashMap<String, Instant>,
) -> io::Result<ReadState> {
    let host = request.header("host").unwrap_or("");
    if request.version == "HTTP/1.1" && host.is_empty() {
        let default_server = &config.servers[server_indices[0]];
        client.response = serialize_reply(
            error_reply(default_server, 400, "HTTP/1.1 requires a Host header"),
            None,
            false,
        );
        client.response_offset = 0;
        client.keep_alive = false;
        client.close_after_write = true;
        client.request_bytes = request.consumed;
        return Ok(ReadState::Ready);
    }
    let selected_server = config.server_index_for_host(server_indices, host);
    let server = &config.servers[selected_server];
    client.server_index = selected_server;
    client.request_bytes = request.consumed;
    client.requests_served += 1;
    client.keep_alive =
        wants_keep_alive(&request) && client.requests_served < MAX_REQUESTS_PER_CONNECTION;
    client.close_after_write = !client.keep_alive;
    if request.body.len() > server.max_body {
        client.keep_alive = false;
        client.close_after_write = true;
        client.response = serialize_reply(
            error_reply(server, 413, "Request body is too large"),
            None,
            false,
        );
        client.response_offset = 0;
        return Ok(ReadState::Ready);
    }
    let cookie = request
        .header("cookie")
        .and_then(|value| cookie_value(value, "localhost_session"));
    let (session_id, set_cookie) = if let Some(id) = cookie.filter(|id| sessions.contains_key(*id))
    {
        sessions.insert(id.to_string(), Instant::now());
        (id.to_string(), false)
    } else {
        let id = create_session_id(sessions.len());
        sessions.insert(id.clone(), Instant::now());
        (id, true)
    };
    let mut job = None;
    let reply = dispatch(config, server, &request, &client.peer_addr, &mut job);
    if let Some(job) = job {
        client.cgi = Some(job);
        client.cgi_set_cookie = set_cookie.then_some(session_id);
        client.writing = false;
        return Ok(ReadState::WaitingCgi);
    }
    client.response = serialize_reply(
        reply,
        set_cookie.then_some(session_id.as_str()),
        client.keep_alive,
    );
    client.response_offset = 0;
    Ok(ReadState::Ready)
}

fn write_response(client: &mut Client) -> io::Result<bool> {
    while client.response_offset < client.response.len() {
        match client
            .stream
            .write(&client.response[client.response_offset..])
        {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "socket closed while writing",
                ))
            }
            Ok(written) => client.response_offset += written,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(true)
}

fn dispatch(
    config: &Config,
    server: &ServerConfig,
    request: &Request,
    peer_addr: &str,
    cgi_job: &mut Option<CgiJob>,
) -> Reply {
    let (raw_path, query) = request.path_and_query();
    let path = match decode_url_path(raw_path) {
        Ok(path) => path,
        Err(()) => return error_reply(server, 400, "Invalid URL path"),
    };
    let route = config.matching_route(server, &path);
    if route.is_none() {
        return error_reply(server, 404, "No route matches this URL");
    }
    let route = route.unwrap();
    if let Some((status, target)) = &route.redirect {
        let mut reply = simple_reply(*status, Vec::new(), "text/plain; charset=utf-8");
        reply
            .extra_headers
            .push(("Location".to_string(), target.clone()));
        return reply;
    }
    if !route.methods.contains(&request.method.to_ascii_uppercase()) {
        let mut reply = error_reply(server, 405, "Method is not allowed for this route");
        reply.extra_headers.push((
            "Allow".to_string(),
            route.methods.iter().cloned().collect::<Vec<_>>().join(", "),
        ));
        return reply;
    }
    let root = route.root.as_deref().unwrap_or(&server.root);
    let relative = relative_route_path(&route.prefix, &path);
    let target = match safe_join(root, &relative) {
        Ok(target) => target,
        Err(()) => return error_reply(server, 403, "Path is outside the configured root"),
    };

    if matches!(request.method.as_str(), "GET" | "POST") {
        if let Some(extension) = target
            .extension()
            .and_then(|value| value.to_str())
            .map(|value| format!(".{value}").to_ascii_lowercase())
        {
            if let Some(interpreter) = server.cgi.get(&extension) {
                let headers = request
                    .headers
                    .iter()
                    .map(|(name, value)| (name.clone(), value.clone()))
                    .collect::<Vec<_>>();
                let path_info = target.to_string_lossy().into_owned();
                let cgi_request = CgiRequest {
                    interpreter,
                    script_path: &target,
                    method: &request.method,
                    request_uri: &request.target,
                    query_string: query,
                    script_name: &path,
                    path_info: &path_info,
                    headers: &headers,
                    body: &request.body,
                    server_name: &server.server_name,
                    server_port: server.port,
                    remote_addr: peer_addr,
                };
                return match CgiJob::spawn(&cgi_request, Duration::from_secs(5)) {
                    Ok(job) => {
                        *cgi_job = Some(job);
                        simple_reply(200, Vec::new(), "text/plain; charset=utf-8")
                    }
                    Err(status) => error_reply(server, status, "Could not start CGI script"),
                };
            }
        }
    }

    match request.method.as_str() {
        "GET" => serve_get(server, route, target),
        "POST" => serve_post(server, route, root, target, request),
        "DELETE" => serve_delete(server, root, target),
        _ => error_reply(server, 405, "Method is not supported"),
    }
}

fn serve_get(server: &ServerConfig, route: &Route, target: PathBuf) -> Reply {
    if target.is_dir() {
        let index = route.index.as_deref().unwrap_or(&server.index);
        let index_path = target.join(index);
        if index_path.is_file() {
            return read_static_file(server, &index_path);
        }
        let listing = route.directory_listing.unwrap_or(server.directory_listing);
        if listing {
            return directory_listing(server, &target);
        }
        return error_reply(
            server,
            403,
            "Directory listing is disabled and index file is missing",
        );
    }
    if let Some(extension) = target
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| format!(".{value}").to_ascii_lowercase())
    {
        if server.cgi.contains_key(&extension) {
            return error_reply(
                server,
                501,
                "Configured CGI execution is not implemented yet",
            );
        }
    }
    read_static_file(server, &target)
}

fn read_static_file(server: &ServerConfig, path: &Path) -> Reply {
    if fs::metadata(path)
        .map(|metadata| metadata.len() > MAX_STATIC_FILE_BYTES)
        .unwrap_or(false)
    {
        return error_reply(server, 500, "Static file exceeds the 64 MiB response limit");
    }
    match fs::read(path) {
        Ok(body) => simple_reply(200, body, &mime_type(path)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            error_reply(server, 404, "File not found")
        }
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            error_reply(server, 403, "File access denied")
        }
        Err(_) => error_reply(server, 500, "Could not read file"),
    }
}

fn serve_post(
    server: &ServerConfig,
    route: &Route,
    root: &Path,
    mut target: PathBuf,
    request: &Request,
) -> Reply {
    if !route.upload {
        return error_reply(server, 403, "File uploads are disabled for this route");
    }
    let content_type = request.header("content-type").unwrap_or("");
    if content_type
        .to_ascii_lowercase()
        .starts_with("multipart/form-data")
    {
        match multipart_file(content_type, &request.body) {
            Some((filename, bytes)) => {
                target = root.join(filename);
                if let Err(reply) = ensure_safe_upload_path(root, &target) {
                    return reply;
                }
                match write_upload(&target, &bytes) {
                    Ok(()) => simple_reply(
                        201,
                        b"File uploaded\n".to_vec(),
                        "text/plain; charset=utf-8",
                    ),
                    Err(_) => error_reply(server, 500, "Could not save uploaded file"),
                }
            }
            None => error_reply(
                server,
                400,
                "Multipart request does not contain a valid file field",
            ),
        }
    } else {
        if target == root || target.is_dir() {
            target.push("upload.bin");
        }
        if let Err(reply) = ensure_safe_upload_path(root, &target) {
            return reply;
        }
        match write_upload(&target, &request.body) {
            Ok(()) => simple_reply(
                201,
                b"File uploaded\n".to_vec(),
                "text/plain; charset=utf-8",
            ),
            Err(_) => error_reply(server, 500, "Could not save uploaded file"),
        }
    }
}

fn serve_delete(server: &ServerConfig, _root: &Path, target: PathBuf) -> Reply {
    match fs::remove_file(target) {
        Ok(()) => simple_reply(204, Vec::new(), "text/plain; charset=utf-8"),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            error_reply(server, 404, "File not found")
        }
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            error_reply(server, 403, "File cannot be deleted")
        }
        Err(_) => error_reply(server, 500, "Could not delete file"),
    }
}

fn write_upload(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, bytes)
}

fn ensure_safe_upload_path(root: &Path, target: &Path) -> Result<(), Reply> {
    let canonical_root = root
        .canonicalize()
        .map_err(|_| simple_reply(500, b"Invalid upload root".to_vec(), "text/plain"))?;
    let parent = target
        .parent()
        .ok_or_else(|| simple_reply(403, b"Invalid upload path".to_vec(), "text/plain"))?;
    let mut existing_parent = parent;
    while !existing_parent.exists() {
        existing_parent = existing_parent
            .parent()
            .ok_or_else(|| simple_reply(403, b"Invalid upload directory".to_vec(), "text/plain"))?;
    }
    let canonical_existing = existing_parent
        .canonicalize()
        .map_err(|_| simple_reply(403, b"Invalid upload directory".to_vec(), "text/plain"))?;
    if !canonical_existing.starts_with(&canonical_root) {
        return Err(simple_reply(
            403,
            b"Upload path is outside the configured root".to_vec(),
            "text/plain",
        ));
    }
    fs::create_dir_all(parent).map_err(|_| {
        simple_reply(
            403,
            b"Upload directory cannot be created".to_vec(),
            "text/plain",
        )
    })?;
    let canonical_parent = parent
        .canonicalize()
        .map_err(|_| simple_reply(403, b"Invalid upload directory".to_vec(), "text/plain"))?;
    if !canonical_parent.starts_with(&canonical_root) {
        return Err(simple_reply(
            403,
            b"Upload path is outside the configured root".to_vec(),
            "text/plain",
        ));
    }
    if target.exists()
        && target
            .canonicalize()
            .is_ok_and(|path| !path.starts_with(&canonical_root))
    {
        return Err(simple_reply(
            403,
            b"Upload target is outside the configured root".to_vec(),
            "text/plain",
        ));
    }
    Ok(())
}

fn multipart_file(content_type: &str, body: &[u8]) -> Option<(String, Vec<u8>)> {
    let boundary = content_type
        .split(';')
        .skip(1)
        .filter_map(|part| part.trim().split_once('='))
        .find(|(key, _)| key.trim().eq_ignore_ascii_case("boundary"))?
        .1
        .trim()
        .trim_matches('"');
    if boundary.is_empty() {
        return None;
    }
    let marker = format!("--{boundary}").into_bytes();
    let mut cursor = find_bytes(body, &marker)? + marker.len();
    while cursor < body.len() {
        if body.get(cursor..cursor + 2) == Some(b"--") {
            return None;
        }
        if body.get(cursor..cursor + 2) != Some(b"\r\n") {
            return None;
        }
        cursor += 2;
        let header_end = find_bytes(&body[cursor..], b"\r\n\r\n")? + cursor;
        let header_text = std::str::from_utf8(&body[cursor..header_end]).ok()?;
        let filename = header_text.lines().find_map(|line| {
            let lower = line.to_ascii_lowercase();
            let position = lower.find("filename=")?;
            let value = line[position + 9..].trim().trim_matches('"');
            let name = Path::new(value).file_name()?.to_str()?.to_string();
            (!name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\']))
                .then_some(name)
        });
        let data_start = header_end + 4;
        let delimiter = [b"\r\n".as_slice(), marker.as_slice()].concat();
        let next = find_bytes(&body[data_start..], &delimiter)? + data_start;
        if let Some(filename) = filename {
            return Some((filename, body[data_start..next].to_vec()));
        }
        cursor = next + 2;
    }
    None
}

fn directory_listing(server: &ServerConfig, directory: &Path) -> Reply {
    let mut entries = match fs::read_dir(directory) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>(),
        Err(_) => return error_reply(server, 403, "Directory cannot be listed"),
    };
    entries.sort();
    let mut html = String::from("<!doctype html><meta charset=\"utf-8\"><title>Directory</title><h1>Directory listing</h1><ul>");
    for entry in entries {
        let safe = html_escape(&entry);
        html.push_str(&format!("<li><a href=\"{safe}\">{safe}</a></li>"));
    }
    html.push_str("</ul>");
    simple_reply(200, html.into_bytes(), "text/html; charset=utf-8")
}

fn safe_join(root: &Path, relative: &str) -> Result<PathBuf, ()> {
    let relative_path = Path::new(relative);
    if relative_path
        .components()
        .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
    {
        return Err(());
    }
    let canonical_root = root.canonicalize().map_err(|_| ())?;
    let candidate = canonical_root.join(relative_path);
    if candidate.exists() {
        let canonical = candidate.canonicalize().map_err(|_| ())?;
        if !canonical.starts_with(&canonical_root) {
            return Err(());
        }
        Ok(canonical)
    } else {
        Ok(candidate)
    }
}

fn relative_route_path(prefix: &str, path: &str) -> String {
    if prefix == "/" {
        path.trim_start_matches('/').to_string()
    } else {
        path.strip_prefix(prefix)
            .unwrap_or("")
            .trim_start_matches('/')
            .to_string()
    }
}

fn decode_url_path(path: &str) -> Result<String, ()> {
    let bytes = path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return Err(());
            }
            let high = hex_value(bytes[index + 1]).ok_or(())?;
            let low = hex_value(bytes[index + 2]).ok_or(())?;
            decoded.push(high * 16 + low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    if decoded
        .iter()
        .any(|byte| *byte == 0 || *byte == b'\\' || *byte < 0x20)
    {
        return Err(());
    }
    let path = String::from_utf8(decoded).map_err(|_| ())?;
    if path.split('/').any(|part| part == "..") {
        return Err(());
    }
    Ok(path)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn mime_type(path: &Path) -> String {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "json" => "application/json",
        "txt" => "text/plain; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "pdf" => "application/pdf",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
    .to_string()
}

fn error_reply(server: &ServerConfig, status: u16, fallback: &str) -> Reply {
    let body = server.error_pages.get(&status).and_then(|path| fs::read(path).ok())
        .unwrap_or_else(|| format!("<!doctype html><meta charset=\"utf-8\"><title>{status} {}</title><h1>{status} {}</h1><p>{}</p>", status_reason(status), status_reason(status), html_escape(fallback)).into_bytes());
    simple_reply(status, body, "text/html; charset=utf-8")
}

fn simple_reply(status: u16, body: Vec<u8>, content_type: &str) -> Reply {
    Reply {
        status,
        body,
        content_type: content_type.to_string(),
        extra_headers: Vec::new(),
    }
}

fn cgi_reply(response: CgiResponse) -> Reply {
    let mut content_type = "text/plain; charset=utf-8".to_string();
    let mut extra_headers = Vec::new();
    for (name, value) in response.headers {
        if name.eq_ignore_ascii_case("content-type") {
            content_type = value;
        } else {
            extra_headers.push((name, value));
        }
    }
    let body = if matches!(response.status, 204 | 304) {
        Vec::new()
    } else {
        response.body
    };
    Reply {
        status: response.status,
        body,
        content_type,
        extra_headers,
    }
}

fn wants_keep_alive(request: &Request) -> bool {
    let connection = request.header("connection").unwrap_or("");
    let tokens = connection
        .split(',')
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>();
    if request.version == "HTTP/1.0" {
        tokens.iter().any(|token| token == "keep-alive")
    } else {
        !tokens.iter().any(|token| token == "close")
    }
}

fn serialize_reply(reply: Reply, set_cookie: Option<&str>, keep_alive: bool) -> Vec<u8> {
    let connection = if keep_alive {
        "Connection: keep-alive\r\nKeep-Alive: timeout=10, max=100\r\n"
    } else {
        "Connection: close\r\n"
    };
    let mut output = format!(
        "HTTP/1.1 {} {}\r\nServer: LocalHost/0.1\r\nContent-Length: {}\r\nContent-Type: {}\r\n{}",
        reply.status,
        status_reason(reply.status),
        reply.body.len(),
        reply.content_type,
        connection
    )
    .into_bytes();
    for (name, value) in reply.extra_headers {
        if !name.contains(['\r', '\n']) && !value.contains(['\r', '\n']) {
            output.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
        }
    }
    if let Some(session_id) = set_cookie {
        output.extend_from_slice(
            format!(
                "Set-Cookie: localhost_session={session_id}; Path=/; HttpOnly; SameSite=Lax\r\n"
            )
            .as_bytes(),
        );
    }
    output.extend_from_slice(b"\r\n");
    output.extend_from_slice(&reply.body);
    output
}

fn cookie_value<'a>(header: &'a str, name: &str) -> Option<&'a str> {
    header
        .split(';')
        .filter_map(|part| part.trim().split_once('='))
        .find_map(|(key, value)| (key == name).then_some(value))
}

fn create_session_id(counter: usize) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "{:032x}",
        nanos ^ ((std::process::id() as u128) << 64) ^ counter as u128
    )
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn status_for_io(error: &io::Error) -> u16 {
    match error.kind() {
        io::ErrorKind::InvalidData => 400,
        io::ErrorKind::PermissionDenied => 403,
        io::ErrorKind::NotFound => 404,
        _ => 500,
    }
}

#[cfg(target_os = "linux")]
fn raw_socket_listener(listener: &TcpListener) -> i32 {
    use std::os::fd::AsRawFd;
    listener.as_raw_fd()
}
#[cfg(target_os = "linux")]
fn raw_socket_stream(stream: &TcpStream) -> i32 {
    use std::os::fd::AsRawFd;
    stream.as_raw_fd()
}
#[cfg(windows)]
fn raw_socket_listener(listener: &TcpListener) -> usize {
    use std::os::windows::io::AsRawSocket;
    listener.as_raw_socket() as usize
}
#[cfg(windows)]
fn raw_socket_stream(stream: &TcpStream) -> usize {
    use std::os::windows::io::AsRawSocket;
    stream.as_raw_socket() as usize
}

#[derive(Clone, Copy)]
struct ReadyEvent {
    token: u64,
    readable: bool,
    writable: bool,
    error: bool,
}

#[cfg(target_os = "linux")]
struct Poller {
    descriptor: i32,
}

#[cfg(target_os = "linux")]
impl Poller {
    fn new() -> io::Result<Self> {
        let descriptor = unsafe { libc::epoll_create1(0) };
        if descriptor < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { descriptor })
    }
    fn add(&mut self, token: u64, socket: i32, interest: u32) -> io::Result<()> {
        self.change(libc::EPOLL_CTL_ADD, token, socket, interest)
    }
    fn modify(&mut self, token: u64, socket: i32, interest: u32) -> io::Result<()> {
        self.change(libc::EPOLL_CTL_MOD, token, socket, interest)
    }
    fn delete(&mut self, token: u64, socket: i32) {
        let _ = token;
        unsafe {
            libc::epoll_ctl(
                self.descriptor,
                libc::EPOLL_CTL_DEL,
                socket,
                std::ptr::null_mut(),
            );
        }
    }
    fn change(&mut self, operation: i32, token: u64, socket: i32, interest: u32) -> io::Result<()> {
        let mut event = libc::epoll_event {
            events: epoll_flags(interest),
            u64: token,
        };
        let result = unsafe { libc::epoll_ctl(self.descriptor, operation, socket, &mut event) };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
    fn wait(&mut self, timeout_ms: i32) -> io::Result<Vec<ReadyEvent>> {
        let mut events = [libc::epoll_event { events: 0, u64: 0 }; 128];
        let count = unsafe {
            libc::epoll_wait(
                self.descriptor,
                events.as_mut_ptr(),
                events.len() as i32,
                timeout_ms,
            )
        };
        if count < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                return Ok(Vec::new());
            }
            return Err(error);
        }
        Ok(events[..count as usize]
            .iter()
            .map(|event| ReadyEvent {
                token: event.u64,
                readable: event.events & ((libc::EPOLLIN | libc::EPOLLHUP) as u32) != 0,
                writable: event.events & (libc::EPOLLOUT as u32) != 0,
                error: event.events & (libc::EPOLLERR as u32) != 0,
            })
            .collect())
    }
}

#[cfg(target_os = "linux")]
impl Drop for Poller {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.descriptor);
        }
    }
}

#[cfg(target_os = "linux")]
fn epoll_flags(interest: u32) -> u32 {
    let mut flags = 0;
    if interest & READABLE != 0 {
        flags |= libc::EPOLLIN as u32;
    }
    if interest & WRITABLE != 0 {
        flags |= libc::EPOLLOUT as u32;
    }
    flags
}

#[cfg(windows)]
struct Poller {
    sockets: HashMap<u64, (usize, u32)>,
}

#[cfg(windows)]
#[repr(C)]
struct WsaPollFd {
    fd: usize,
    events: i16,
    revents: i16,
}

#[cfg(windows)]
#[link(name = "ws2_32")]
extern "system" {
    fn WSAPoll(fds: *mut WsaPollFd, count: u32, timeout: i32) -> i32;
}

#[cfg(windows)]
impl Poller {
    fn new() -> io::Result<Self> {
        Ok(Self {
            sockets: HashMap::new(),
        })
    }
    fn add(&mut self, token: u64, socket: usize, interest: u32) -> io::Result<()> {
        self.sockets.insert(token, (socket, interest));
        Ok(())
    }
    fn modify(&mut self, token: u64, socket: usize, interest: u32) -> io::Result<()> {
        self.sockets.insert(token, (socket, interest));
        Ok(())
    }
    fn delete(&mut self, token: u64, _socket: usize) {
        self.sockets.remove(&token);
    }
    fn wait(&mut self, timeout_ms: i32) -> io::Result<Vec<ReadyEvent>> {
        const POLLRDNORM: i16 = 0x0100;
        const POLLWRNORM: i16 = 0x0010;
        const POLLERR: i16 = 0x0001;
        const POLLHUP: i16 = 0x0002;
        const POLLNVAL: i16 = 0x0004;
        let tokens = self.sockets.keys().copied().collect::<Vec<_>>();
        let mut fds = tokens
            .iter()
            .map(|token| {
                let (fd, interest) = self.sockets[token];
                let mut events = 0;
                if interest & READABLE != 0 {
                    events |= POLLRDNORM;
                }
                if interest & WRITABLE != 0 {
                    events |= POLLWRNORM;
                }
                WsaPollFd {
                    fd,
                    events,
                    revents: 0,
                }
            })
            .collect::<Vec<_>>();
        if fds.is_empty() {
            std::thread::sleep(Duration::from_millis(timeout_ms.max(0) as u64));
            return Ok(Vec::new());
        }
        let count = unsafe { WSAPoll(fds.as_mut_ptr(), fds.len() as u32, timeout_ms) };
        if count == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(fds
            .iter()
            .enumerate()
            .filter(|(_, fd)| fd.revents != 0)
            .map(|(index, fd)| ReadyEvent {
                token: tokens[index],
                readable: fd.revents & (POLLRDNORM | POLLHUP) != 0,
                writable: fd.revents & POLLWRNORM != 0,
                error: fd.revents & (POLLERR | POLLNVAL) != 0,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::{decode_url_path, dispatch, multipart_file, relative_route_path, wants_keep_alive};
    use crate::config::Config;
    use crate::http::Request;
    use std::collections::HashMap;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn fixture_config() -> (Config, PathBuf) {
        let id = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base =
            std::env::temp_dir().join(format!("localhost-server-{}-{id}", std::process::id()));
        fs::create_dir_all(base.join("www")).unwrap();
        let source = "server address=127.0.0.1 port=8080 name=localhost root=www\nroute / methods=GET root=www\n";
        (Config::parse(source, &base).unwrap(), base)
    }

    fn request(method: &str, target: &str) -> Request {
        Request {
            method: method.to_string(),
            target: target.to_string(),
            version: "HTTP/1.1".to_string(),
            headers: HashMap::new(),
            body: Vec::new(),
            consumed: 0,
        }
    }

    #[test]
    fn keep_alive_defaults_follow_http_version_and_connection_header() {
        let mut http11 = request("GET", "/");
        assert!(wants_keep_alive(&http11));
        http11
            .headers
            .insert("connection".to_string(), "upgrade, close".to_string());
        assert!(!wants_keep_alive(&http11));

        let mut http10 = request("GET", "/");
        http10.version = "HTTP/1.0".to_string();
        assert!(!wants_keep_alive(&http10));
        http10
            .headers
            .insert("connection".to_string(), "keep-alive".to_string());
        assert!(wants_keep_alive(&http10));
    }

    #[test]
    fn blocks_path_traversal_and_decodes_safe_paths() {
        assert_eq!(decode_url_path("/hello%20world").unwrap(), "/hello world");
        assert!(decode_url_path("/%2e%2e/secret").is_err());
        assert_eq!(relative_route_path("/uploads", "/uploads/a.txt"), "a.txt");
    }

    #[test]
    fn extracts_multipart_file_content() {
        let body = b"--xyz\r\nContent-Disposition: form-data; name=\"file\"; filename=\"note.txt\"\r\nContent-Type: text/plain\r\n\r\nhello\r\n--xyz--\r\n";
        let (name, bytes) = multipart_file("multipart/form-data; boundary=xyz", body).unwrap();
        assert_eq!(name, "note.txt");
        assert_eq!(bytes, b"hello");
    }

    #[test]
    fn generates_route_statuses_for_missing_files_and_disallowed_methods() {
        let (config, base) = fixture_config();
        let server = &config.servers[0];
        let mut job = None;
        assert_eq!(
            dispatch(
                &config,
                server,
                &request("GET", "/missing.txt"),
                "127.0.0.1",
                &mut job,
            )
            .status,
            404
        );
        assert_eq!(
            dispatch(
                &config,
                server,
                &request("POST", "/"),
                "127.0.0.1",
                &mut job,
            )
            .status,
            405
        );
        assert_eq!(
            dispatch(
                &config,
                server,
                &request("GET", "/%2e%2e/secret"),
                "127.0.0.1",
                &mut job,
            )
            .status,
            400
        );
        fs::remove_dir_all(base).unwrap();
    }
}
