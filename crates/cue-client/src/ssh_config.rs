use std::collections::BTreeSet;

use crate::{home_dir, read_config_source};

pub fn detected_ssh_hosts() -> BTreeSet<String> {
    let path = home_dir().join(".ssh/config");
    let text = match read_config_source(&path) {
        Ok(Some(text)) => text,
        Ok(None) => return BTreeSet::new(),
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "failed to read ssh config");
            return BTreeSet::new();
        }
    };

    parse_ssh_config_hosts(&text)
}

fn parse_ssh_config_hosts(text: &str) -> BTreeSet<String> {
    let mut hosts = BTreeSet::new();

    for line in text.lines() {
        let line = strip_comment(line).trim();
        if line.is_empty() {
            continue;
        }

        let mut parts = line.split_whitespace();
        let Some(keyword) = parts.next() else {
            continue;
        };
        if !keyword.eq_ignore_ascii_case("host") {
            continue;
        }

        for host in parts {
            if is_explicit_host(host) {
                hosts.insert(host.to_string());
            }
        }
    }

    hosts
}

fn strip_comment(line: &str) -> &str {
    line.split('#').next().unwrap_or(line)
}

fn is_explicit_host(host: &str) -> bool {
    !host.is_empty()
        && host != "*"
        && !host.starts_with('!')
        && !host.contains('*')
        && !host.contains('?')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_explicit_hosts_and_ignores_patterns() {
        let hosts = parse_ssh_config_hosts(
            r#"
Host *
  ServerAliveInterval 30

Host devbox prod-box
  User me

Host !banned *.corp ???
  User ignored

Host nested # trailing comment
  HostName example.com
"#,
        );

        assert_eq!(
            hosts.into_iter().collect::<Vec<_>>(),
            vec![
                "devbox".to_string(),
                "nested".to_string(),
                "prod-box".to_string()
            ]
        );
    }
}
