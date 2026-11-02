use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

pub const MAX_ALLOWED_BODY: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct Config {
    pub servers: Vec<ServerConfig>,
    pub base_dir: PathBuf,
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub address: String,
    pub port: u16,
    pub server_name: String,
    pub root: PathBuf,
    pub max_body: usize,
    pub index: String,
    pub directory_listing: bool,
    pub routes: Vec<Route>,
    pub error_pages: HashMap<u16, PathBuf>,
    pub cgi: HashMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct Route {
    pub prefix: String,
    pub methods: BTreeSet<String>,
    pub root: Option<PathBuf>,
    pub redirect: Option<(u16, String)>,
    pub directory_listing: Option<bool>,
    pub index: Option<String>,
    pub upload: bool,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, String> {
        let source = fs::read_to_string(path)
            .map_err(|error| format!("cannot read config {}: {error}", path.display()))?;
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let base_dir = parent.canonicalize().map_err(|error| {
            format!(
                "cannot resolve config directory {}: {error}",
                parent.display()
            )
        })?;
        Self::parse(&source, &base_dir)
    }

    pub fn parse(source: &str, base_dir: &Path) -> Result<Self, String> {
        let mut config = Config {
            servers: Vec::new(),
            base_dir: base_dir.to_path_buf(),
        };
        let mut current: Option<usize> = None;
        for (line_number, raw_line) in source.lines().enumerate() {
            let line = raw_line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let mut words = line.split_ascii_whitespace();
            let directive = words.next().unwrap_or("");
            let line_error = |message: &str| format!("config line {}: {message}", line_number + 1);
            match directive {
                "server" => {
                    let values = key_values(words, &line_error)?;
                    let address = required(&values, "address", &line_error)?;
                    let port = required(&values, "port", &line_error)?
                        .parse::<u16>()
                        .map_err(|_| line_error("port must be an integer from 1 to 65535"))?;
                    if port == 0 {
                        return Err(line_error("port must be from 1 to 65535"));
                    }
                    let server_name = values
                        .get("name")
                        .cloned()
                        .unwrap_or_else(|| "_".to_string());
                    let root_value = required(&values, "root", &line_error)?;
                    let root = resolve_path(base_dir, &root_value);
                    let max_body = values.get("max_body").map_or(Ok(1024 * 1024), |value| {
                        value
                            .parse::<usize>()
                            .map_err(|_| line_error("max_body must be a positive integer"))
                    })?;
                    if max_body == 0 || max_body > MAX_ALLOWED_BODY {
                        return Err(line_error("max_body must be between 1 and 16777216 bytes"));
                    }
                    let index = values
                        .get("index")
                        .cloned()
                        .unwrap_or_else(|| "index.html".to_string());
                    if !safe_index(&index) {
                        return Err(line_error(
                            "index must be a file name without parent-directory components",
                        ));
                    }
                    let directory_listing = values
                        .get("listing")
                        .map_or(Ok(false), |v| parse_bool(v, &line_error))?;
                    config.servers.push(ServerConfig {
                        address,
                        port,
                        server_name,
                        root,
                        max_body,
                        index,
                        directory_listing,
                        routes: Vec::new(),
                        error_pages: HashMap::new(),
                        cgi: HashMap::new(),
                    });
                    current = Some(config.servers.len() - 1);
                }
                "route" => {
                    let prefix = words
                        .next()
                        .ok_or_else(|| line_error("route requires a URL prefix"))?
                        .to_string();
                    if !prefix.starts_with('/') || prefix.contains('?') || prefix.contains('#') {
                        return Err(line_error("route prefix must be an absolute URL path"));
                    }
                    let values = key_values(words, &line_error)?;
                    let mut methods = BTreeSet::new();
                    for method in values
                        .get("methods")
                        .map(String::as_str)
                        .unwrap_or("GET,POST,DELETE")
                        .split(',')
                    {
                        let method = method.to_ascii_uppercase();
                        if !matches!(method.as_str(), "GET" | "POST" | "DELETE") {
                            return Err(line_error("methods may contain only GET, POST, DELETE"));
                        }
                        methods.insert(method);
                    }
                    if methods.is_empty() {
                        return Err(line_error("route must allow at least one method"));
                    }
                    let root = values
                        .get("root")
                        .map(|value| resolve_path(base_dir, value));
                    let redirect = values.get("redirect").map(|target| {
                        let status = values.get("status").map_or(Ok(301u16), |value| {
                            value.parse::<u16>().map_err(|_| line_error("redirect status must be 301 or 302"))
                        })?;
                        if !matches!(status, 301 | 302) || !(target.starts_with('/') || target.starts_with("http://") || target.starts_with("https://")) {
                            return Err(line_error("redirect requires status 301 or 302 and a local or absolute HTTP URL"));
                        }
                        Ok((status, target.clone()))
                    }).transpose()?;
                    let directory_listing = values
                        .get("listing")
                        .map(|v| parse_bool(v, &line_error))
                        .transpose()?;
                    let index = values.get("index").cloned();
                    if index.as_deref().is_some_and(|value| !safe_index(value)) {
                        return Err(line_error("route index must be a file name"));
                    }
                    let upload = values
                        .get("upload")
                        .map_or(Ok(false), |v| parse_bool(v, &line_error))?;
                    let server = current_server_mut(&mut config, current, &line_error)?;
                    server.routes.push(Route {
                        prefix,
                        methods,
                        root,
                        redirect,
                        directory_listing,
                        index,
                        upload,
                    });
                }
                "error" => {
                    let pair = words
                        .next()
                        .ok_or_else(|| line_error("error requires CODE=PATH"))?;
                    if words.next().is_some() {
                        return Err(line_error("error accepts one CODE=PATH value"));
                    }
                    let (code, path) = pair
                        .split_once('=')
                        .ok_or_else(|| line_error("error requires CODE=PATH"))?;
                    let code = code
                        .parse::<u16>()
                        .map_err(|_| line_error("invalid error status code"))?;
                    if !matches!(code, 400 | 403 | 404 | 405 | 413 | 500 | 504) {
                        return Err(line_error(
                            "supported error pages are 400, 403, 404, 405, 413, 500 and 504",
                        ));
                    }
                    current_server_mut(&mut config, current, &line_error)?
                        .error_pages
                        .insert(code, resolve_path(base_dir, path));
                }
                "cgi" => {
                    let pair = words
                        .next()
                        .ok_or_else(|| line_error("cgi requires EXTENSION=PROGRAM"))?;
                    if words.next().is_some() {
                        return Err(line_error("cgi accepts one EXTENSION=PROGRAM value"));
                    }
                    let (extension, program) = pair
                        .split_once('=')
                        .ok_or_else(|| line_error("cgi requires EXTENSION=PROGRAM"))?;
                    if !extension.starts_with('.') || extension.len() < 2 || program.is_empty() {
                        return Err(line_error(
                            "CGI extension must start with a dot and program cannot be empty",
                        ));
                    }
                    current_server_mut(&mut config, current, &line_error)?
                        .cgi
                        .insert(extension.to_ascii_lowercase(), program.to_string());
                }
                other => return Err(line_error(&format!("unknown directive '{other}'"))),
            }
        }

        if config.servers.is_empty() {
            return Err("config must contain at least one server directive".to_string());
        }
        config.validate()?;
        Ok(config)
    }

    pub fn matching_route<'a>(&'a self, server: &'a ServerConfig, path: &str) -> Option<&'a Route> {
        server
            .routes
            .iter()
            .filter(|route| {
                path == route.prefix
                    || route.prefix == "/"
                    || path
                        .strip_prefix(&route.prefix)
                        .is_some_and(|tail| tail.starts_with('/'))
            })
            .max_by_key(|route| route.prefix.len())
    }

    pub fn server_for_host<'a>(&'a self, group: &[usize], host: &str) -> &'a ServerConfig {
        &self.servers[self.server_index_for_host(group, host)]
    }

    pub fn server_index_for_host(&self, group: &[usize], host: &str) -> usize {
        let host = host.trim();
        let name = if let Some(bracketed) = host.strip_prefix('[') {
            bracketed
                .split_once(']')
                .map(|(address, _)| address)
                .unwrap_or(bracketed)
        } else if host.matches(':').count() == 1 {
            host.split_once(':').map(|(name, _)| name).unwrap_or(host)
        } else {
            host
        };
        let normalized = name.to_ascii_lowercase();
        group
            .iter()
            .find(|index| {
                let Some(server) = self.servers.get(**index) else {
                    return false;
                };
                server.server_name == "*" || server.server_name.eq_ignore_ascii_case(&normalized)
            })
            .copied()
            .or_else(|| group.first().copied())
            .expect("listener group always has a server")
    }

    fn validate(&self) -> Result<(), String> {
        let mut names = HashMap::<(String, u16), BTreeSet<String>>::new();
        for server in &self.servers {
            if server.address.parse::<IpAddr>().is_err() {
                return Err(format!(
                    "invalid listen address '{}' (use an IP address)",
                    server.address
                ));
            }
            if !server.root.is_dir() {
                return Err(format!(
                    "server root is not an existing directory: {}",
                    server.root.display()
                ));
            }
            let set = names
                .entry((server.address.clone(), server.port))
                .or_default();
            let name = server.server_name.to_ascii_lowercase();
            if !set.insert(name.clone()) {
                return Err(format!(
                    "duplicate server name '{name}' on {}:{}",
                    server.address, server.port
                ));
            }
            for route in &server.routes {
                if let Some(root) = &route.root {
                    if !root.is_dir() {
                        return Err(format!(
                            "route root is not an existing directory: {}",
                            root.display()
                        ));
                    }
                }
            }
            for (code, path) in &server.error_pages {
                if !path.is_file() {
                    return Err(format!(
                        "error page {code} does not exist: {}",
                        path.display()
                    ));
                }
            }
        }
        Ok(())
    }
}

fn current_server_mut<'a>(
    config: &'a mut Config,
    current: Option<usize>,
    line_error: &impl Fn(&str) -> String,
) -> Result<&'a mut ServerConfig, String> {
    let index =
        current.ok_or_else(|| line_error("this directive must follow a server directive"))?;
    config
        .servers
        .get_mut(index)
        .ok_or_else(|| line_error("invalid server section"))
}

fn key_values<'a>(
    words: impl Iterator<Item = &'a str>,
    line_error: &impl Fn(&str) -> String,
) -> Result<HashMap<String, String>, String> {
    let mut values = HashMap::new();
    for word in words {
        let (key, value) = word
            .split_once('=')
            .ok_or_else(|| line_error("expected key=value"))?;
        if key.is_empty() || value.is_empty() {
            return Err(line_error("configuration values cannot be empty"));
        }
        if values.insert(key.to_string(), value.to_string()).is_some() {
            return Err(line_error(&format!("duplicate option '{key}'")));
        }
    }
    Ok(values)
}

fn required(
    values: &HashMap<String, String>,
    key: &str,
    line_error: &impl Fn(&str) -> String,
) -> Result<String, String> {
    values
        .get(key)
        .cloned()
        .ok_or_else(|| line_error(&format!("server requires '{key}='")))
}

fn resolve_path(base_dir: &Path, value: &str) -> PathBuf {
    let path = Path::new(value);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base_dir.join(path)
    }
}

fn parse_bool(value: &str, line_error: &impl Fn(&str) -> String) -> Result<bool, String> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "on" | "yes" => Ok(true),
        "false" | "off" | "no" => Ok(false),
        _ => Err(line_error("expected true or false")),
    }
}

fn safe_index(value: &str) -> bool {
    !value.is_empty()
        && !value.contains('/')
        && !value.contains('\\')
        && value != "."
        && value != ".."
}

#[cfg(test)]
mod tests {
    use super::Config;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn fixture() -> PathBuf {
        let id = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base =
            std::env::temp_dir().join(format!("localhost-config-{}-{id}", std::process::id()));
        fs::create_dir_all(base.join("www")).unwrap();
        base
    }

    #[test]
    fn validates_virtual_hosts_and_matches_longest_route() {
        let base = fixture();
        let source = "server address=127.0.0.1 port=8080 name=one root=www\nroute / methods=GET root=www\nroute /images methods=GET root=www\nserver address=127.0.0.1 port=8080 name=two root=www\n";
        let config = Config::parse(source, &base).unwrap();
        let first = config.server_for_host(&[0, 1], "two:8080");
        assert_eq!(first.server_name, "two");
        let route = config
            .matching_route(&config.servers[0], "/images/a.png")
            .unwrap();
        assert_eq!(route.prefix, "/images");
        assert_eq!(
            config.server_for_host(&[0, 1], "unknown:8080").server_name,
            "one"
        );
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn rejects_duplicate_names_invalid_roots_and_large_limits() {
        let base = fixture();
        let duplicate = "server address=127.0.0.1 port=8080 name=one root=www\nserver address=127.0.0.1 port=8080 name=ONE root=www\n";
        assert!(Config::parse(duplicate, &base)
            .unwrap_err()
            .contains("duplicate server name"));
        let invalid_root = "server address=127.0.0.1 port=8080 name=one root=missing\n";
        assert!(Config::parse(invalid_root, &base)
            .unwrap_err()
            .contains("existing directory"));
        let large = "server address=127.0.0.1 port=8080 name=one root=www max_body=20000000\n";
        assert!(Config::parse(large, &base)
            .unwrap_err()
            .contains("max_body"));
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn both_shipped_platform_configs_parse_with_their_interpreter() {
        let project = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let windows = Config::load(&project.join("localhost.conf")).unwrap();
        let linux = Config::load(&project.join("localhost.linux.conf")).unwrap();
        assert_eq!(windows.servers[0].cgi[".py"], "py");
        assert_eq!(linux.servers[0].cgi[".py"], "python3");
    }
}
