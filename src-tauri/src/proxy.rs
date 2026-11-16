use reqwest::{blocking::ClientBuilder, Proxy};
use serde::{Deserialize, Serialize};
use std::process::Command;
use url::Url;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", tag = "mode", content = "url")]
pub enum ProxySettings {
    #[default]
    System,
    Direct,
    Custom(String),
}

impl ProxySettings {
    pub fn validated(self) -> Result<Self, String> {
        let Self::Custom(address) = self else {
            return Ok(self);
        };
        let url = Url::parse(address.trim())
            .map_err(|_| "Enter a complete proxy URL, such as http://127.0.0.1:8080.")?;
        if !matches!(url.scheme(), "http" | "https" | "socks5" | "socks5h")
            || url.host().is_none()
            || url.port_or_known_default().is_none()
            || !matches!(url.path(), "" | "/")
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err("Enter an HTTP, HTTPS, SOCKS5, or SOCKS5h proxy host and port.".into());
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err("Enter a proxy URL without a username or password.".into());
        }
        Ok(Self::Custom(url.to_string()))
    }

    pub fn resolve(&self) -> ResolvedProxy {
        match self {
            Self::Direct => ResolvedProxy::Direct,
            Self::Custom(url) => ResolvedProxy::Custom(url.clone()),
            Self::System => {
                #[cfg(target_os = "linux")]
                if let Some(proxy) = gnome_manual_proxy() {
                    return proxy;
                }
                ResolvedProxy::System
            }
        }
    }
}

pub enum ResolvedProxy {
    System,
    Direct,
    Custom(String),
    #[cfg(target_os = "linux")]
    Desktop {
        http: Option<String>,
        https: Option<String>,
        socks: Option<String>,
        bypass: String,
    },
}

impl ResolvedProxy {
    pub fn configure_client(&self, client: ClientBuilder) -> Result<ClientBuilder, String> {
        let proxy_error = |_| "Cannot initialize the configured proxy.".to_string();
        match self {
            Self::System => Ok(client),
            Self::Direct => Ok(client.no_proxy()),
            Self::Custom(url) => Ok(client.proxy(Proxy::all(url).map_err(proxy_error)?)),
            #[cfg(target_os = "linux")]
            Self::Desktop {
                http,
                https,
                socks,
                bypass,
            } => {
                let mut client = client.no_proxy();
                let bypass = reqwest::NoProxy::from_string(bypass);
                if let Some(url) = http.as_ref().or(socks.as_ref()) {
                    client = client.proxy(
                        Proxy::http(url)
                            .map_err(proxy_error)?
                            .no_proxy(bypass.clone()),
                    );
                }
                if let Some(url) = https.as_ref().or(socks.as_ref()) {
                    client = client.proxy(Proxy::https(url).map_err(proxy_error)?.no_proxy(bypass));
                }
                Ok(client)
            }
        }
    }

    pub fn configure_command(&self, command: &mut Command) {
        match self {
            Self::System => {}
            Self::Direct => {
                command.args(["--proxy", ""]);
            }
            Self::Custom(url) => {
                command.args(["--proxy", url]);
            }
            #[cfg(target_os = "linux")]
            Self::Desktop {
                http,
                https,
                socks,
                bypass,
            } => {
                for name in [
                    "HTTP_PROXY",
                    "HTTPS_PROXY",
                    "ALL_PROXY",
                    "NO_PROXY",
                    "http_proxy",
                    "https_proxy",
                    "all_proxy",
                    "no_proxy",
                ] {
                    command.env_remove(name);
                }
                if let Some(url) = http.as_ref().or(socks.as_ref()) {
                    command.env("http_proxy", url);
                }
                if let Some(url) = https.as_ref().or(socks.as_ref()) {
                    command.env("https_proxy", url);
                }
                command.env("no_proxy", bypass);
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn gnome_manual_proxy() -> Option<ResolvedProxy> {
    if [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ]
    .iter()
    .any(|key| std::env::var_os(key).is_some_and(|value| !value.is_empty()))
    {
        return None;
    }
    let schema = "org.gnome.system.proxy";
    if gsettings(schema, "mode").as_deref().map(unquote) != Some("manual") {
        return None;
    }
    let http = gnome_endpoint("http", "http");
    let https = if gsettings(schema, "use-same-proxy").as_deref() == Some("true") {
        http.clone()
    } else {
        gnome_endpoint("https", "http")
    };
    let socks = gnome_endpoint("socks", "socks5h");
    let bypass = gsettings(schema, "ignore-hosts").unwrap_or_default();
    let bypass = bypass
        .trim_start_matches("@as ")
        .trim_matches(['[', ']'])
        .split(',')
        .map(|entry| unquote(entry.trim()).trim_start_matches("*."))
        .collect::<Vec<_>>()
        .join(",");
    Some(ResolvedProxy::Desktop {
        http,
        https,
        socks,
        bypass,
    })
}

#[cfg(target_os = "linux")]
fn gnome_endpoint(protocol: &str, scheme: &str) -> Option<String> {
    let schema = format!("org.gnome.system.proxy.{protocol}");
    let host = gsettings(&schema, "host")?;
    let host = unquote(&host);
    if host.is_empty() {
        return None;
    }
    let port: u16 = gsettings(&schema, "port")?.parse().ok()?;
    if port == 0 {
        return None;
    }
    let host = match host.parse::<std::net::Ipv6Addr>() {
        Ok(address) => format!("[{address}]"),
        Err(_) => host.to_owned(),
    };
    if let Ok(ProxySettings::Custom(url)) =
        ProxySettings::Custom(format!("{scheme}://{host}:{port}")).validated()
    {
        return Some(url);
    }
    None
}

#[cfg(target_os = "linux")]
fn gsettings(schema: &str, key: &str) -> Option<String> {
    let output = Command::new("gsettings")
        .args(["get", schema, key])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

#[cfg(target_os = "linux")]
fn unquote(text: &str) -> &str {
    text.strip_prefix('\'')
        .and_then(|value| value.strip_suffix('\''))
        .or_else(|| {
            text.strip_prefix('"')
                .and_then(|value| value.strip_suffix('"'))
        })
        .unwrap_or(text)
}
