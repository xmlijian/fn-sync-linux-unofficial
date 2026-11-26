//! 连接地址归一化（与前端 `getConnectionTypeAndAddress` 语义一致）。

use super::fnid;
use anyhow::{anyhow, Result};

fn is_ipv4(address: &str) -> bool {
    let host = address.rsplit_once(':').map(|(h, _)| h).unwrap_or(address);
    host.parse::<std::net::Ipv4Addr>().is_ok()
}

fn is_ipv6(address: &str) -> bool {
    if let Some(rest) = address.strip_prefix('[') {
        if let Some((host, tail)) = rest.split_once(']') {
            let port_ok =
                tail.is_empty() || tail.strip_prefix(':').is_some_and(|p| p.parse::<u16>().is_ok());
            return port_ok && host.parse::<std::net::Ipv6Addr>().is_ok();
        }
        return false;
    }
    if address.parse::<std::net::Ipv6Addr>().is_ok() {
        return true;
    }
    address
        .rsplit_once(':')
        .map(|(host, port)| {
            host.parse::<std::net::Ipv6Addr>().is_ok() && port.parse::<u16>().is_ok()
        })
        .unwrap_or(false)
}

fn is_domain(address: &str) -> bool {
    let host = if let Some(rest) = address.strip_prefix('[') {
        return match rest.split_once(']') {
            Some((_, tail)) => tail
                .strip_prefix(':')
                .is_some_and(|p| p.parse::<u16>().is_ok()),
            None => false,
        };
    } else {
        match address.rsplit_once(':') {
            Some((h, p)) if !p.is_empty() && p.parse::<u16>().is_ok() => h,
            _ => address,
        }
    };
    if host.parse::<std::net::Ipv4Addr>().is_ok() || host.parse::<std::net::Ipv6Addr>().is_ok() {
        return false;
    }
    let mut labels = host.split('.');
    let first = labels.next().unwrap_or_default();
    if first.is_empty()
        || !first
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
    {
        return false;
    }
    let rest: Vec<&str> = labels.collect();
    if rest.is_empty() {
        return false;
    }
    rest.iter().all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-')
    })
}

pub fn connection_type(address: &str) -> &'static str {
    if is_ipv4(address) || is_ipv6(address) {
        "ip"
    } else if fnid::extract_fnid(address).is_some() || is_domain(address) {
        "host"
    } else {
        "fnid"
    }
}

fn ensure_port(address: &str) -> String {
    if is_ipv6(address) {
        let has_port = address.starts_with('[') && address.contains("]:");
        if has_port {
            return address.to_string();
        }
        let wrapped = if address.starts_with('[') {
            address.to_string()
        } else {
            format!("[{address}]")
        };
        return format!("{wrapped}:5667");
    }
    if address.contains(':') {
        address.to_string()
    } else {
        format!("{address}:5667")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTarget {
    pub address: String,
    pub conn_type: String,
}

pub fn normalize(address: &str) -> ResolvedTarget {
    let address = address.trim();
    if let Some(id) = fnid::extract_fnid(address) {
        return ResolvedTarget {
            address: id,
            conn_type: "fnid".into(),
        };
    }
    match connection_type(address) {
        "ip" => ResolvedTarget {
            address: ensure_port(address),
            conn_type: "ip".into(),
        },
        "host" => ResolvedTarget {
            address: ensure_port(address),
            conn_type: "host".into(),
        },
        _ => ResolvedTarget {
            address: address.to_string(),
            conn_type: "fnid".into(),
        },
    }
}

pub async fn resolve_target(address: &str, conn_type: &str) -> Result<ResolvedTarget> {
    let normalized = normalize(address);
    let kind = if conn_type.trim().is_empty() {
        normalized.conn_type.as_str()
    } else {
        conn_type.trim()
    };
    match kind {
        "fnid" => {
            let id = if normalized.conn_type == "fnid" {
                normalized.address.clone()
            } else {
                fnid::extract_fnid(address).unwrap_or_else(|| normalized.address.clone())
            };
            let relay = fnid::resolve(&id).await?;
            Ok(ResolvedTarget {
                address: relay,
                conn_type: "fnid".into(),
            })
        }
        "ip" | "host" => {
            if normalized.conn_type == "fnid" {
                let relay = fnid::resolve(&normalized.address).await?;
                return Ok(ResolvedTarget {
                    address: relay,
                    conn_type: "fnid".into(),
                });
            }
            Ok(normalized)
        }
        other => Err(anyhow!("unsupported connection type: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_addresses() {
        assert_eq!(connection_type("10.0.0.5:5667"), "ip");
        assert_eq!(connection_type("10.0.0.5"), "ip");
        assert_eq!(connection_type("nas.example.com:5667"), "host");
        assert_eq!(connection_type("example.org"), "host");
        assert_eq!(connection_type("demo-abc.fnos.net"), "host");
        assert_eq!(connection_type("demo-abc"), "fnid");
        assert_eq!(connection_type("[::1]:5667"), "ip");
        assert_eq!(connection_type("2001:db8::1"), "ip");
    }

    #[test]
    fn normalize_assigns_ports_and_fnid() {
        assert_eq!(
            normalize("10.0.0.5"),
            ResolvedTarget {
                address: "10.0.0.5:5667".into(),
                conn_type: "ip".into()
            }
        );
        assert_eq!(
            normalize("nas.example.com"),
            ResolvedTarget {
                address: "nas.example.com:5667".into(),
                conn_type: "host".into()
            }
        );
        assert_eq!(
            normalize("demo-abc.fnos.net"),
            ResolvedTarget {
                address: "demo-abc".into(),
                conn_type: "fnid".into()
            }
        );
        assert_eq!(
            normalize("demo-abc"),
            ResolvedTarget {
                address: "demo-abc".into(),
                conn_type: "fnid".into()
            }
        );
        assert_eq!(
            normalize("[::1]"),
            ResolvedTarget {
                address: "[::1]:5667".into(),
                conn_type: "ip".into()
            }
        );
    }

    #[test]
    fn ipv4_with_port_is_not_domain() {
        assert!(!is_domain("10.0.0.5:5667"));
        assert!(is_ipv4("10.0.0.5:5667"));
    }
}
