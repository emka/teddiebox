#![no_std]

//! Parses `teddiebox.conf` from the SD card.
//!
//! Format is deliberately the dullest thing that works: `key = value`, one per
//! line, `#` comments, blank lines ignored. A parent editing this file on a
//! laptop should not be able to get the syntax wrong.

use heapless::String;

pub const MAX_SSID: usize = 32;
pub const MAX_PASSWORD: usize = 63;
pub const MAX_SERVER: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub ssid: String<MAX_SSID>,
    pub password: String<MAX_PASSWORD>,
    /// `host:port` of the teddyCloud server.
    pub server: String<MAX_SERVER>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigError {
    MissingSsid,
    MissingServer,
    ValueTooLong,
    MalformedLine,
}

impl Config {
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let mut ssid: Option<String<MAX_SSID>> = None;
        let mut password: String<MAX_PASSWORD> = String::new();
        let mut server: Option<String<MAX_SERVER>> = None;

        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            // Split on the first '=' only, so values may contain more.
            let Some((key, value)) = line.split_once('=') else {
                return Err(ConfigError::MalformedLine);
            };
            let key = key.trim();
            let value = value.trim();

            match key {
                "ssid" => {
                    ssid = Some(String::try_from(value).map_err(|_| ConfigError::ValueTooLong)?);
                }
                "password" => {
                    password = String::try_from(value).map_err(|_| ConfigError::ValueTooLong)?;
                }
                "server" => {
                    server = Some(String::try_from(value).map_err(|_| ConfigError::ValueTooLong)?);
                }
                // Unknown keys are ignored so a newer config file does not
                // brick an older firmware.
                _ => {}
            }
        }

        Ok(Config {
            ssid: ssid.ok_or(ConfigError::MissingSsid)?,
            password,
            server: server.ok_or(ConfigError::MissingServer)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_minimal_file() {
        let c =
            Config::parse("ssid = HomeNet\npassword = hunter2\nserver = 10.0.0.5:8080\n").unwrap();
        assert_eq!(c.ssid.as_str(), "HomeNet");
        assert_eq!(c.password.as_str(), "hunter2");
        assert_eq!(c.server.as_str(), "10.0.0.5:8080");
    }

    #[test]
    fn ignores_comments_and_blank_lines() {
        let text = "# my box\n\nssid = HomeNet\n\n# the server\nserver = box.lan:8080\n";
        let c = Config::parse(text).unwrap();
        assert_eq!(c.ssid.as_str(), "HomeNet");
        assert_eq!(c.server.as_str(), "box.lan:8080");
    }

    #[test]
    fn tolerates_missing_and_extra_whitespace() {
        let c = Config::parse("ssid=HomeNet\n   server   =   box.lan:8080   \n").unwrap();
        assert_eq!(c.ssid.as_str(), "HomeNet");
        assert_eq!(c.server.as_str(), "box.lan:8080");
    }

    #[test]
    fn accepts_windows_line_endings() {
        let c = Config::parse("ssid = HomeNet\r\nserver = box.lan:8080\r\n").unwrap();
        assert_eq!(c.ssid.as_str(), "HomeNet");
        assert_eq!(c.server.as_str(), "box.lan:8080");
    }

    #[test]
    fn an_open_network_needs_no_password() {
        let c = Config::parse("ssid = Cafe\nserver = box.lan:8080\n").unwrap();
        assert!(c.password.is_empty());
    }

    #[test]
    fn a_password_may_contain_equals_signs() {
        let c = Config::parse("ssid = A\npassword = a=b=c\nserver = s:1\n").unwrap();
        assert_eq!(c.password.as_str(), "a=b=c");
    }

    #[test]
    fn a_missing_ssid_is_an_error() {
        assert_eq!(
            Config::parse("server = box.lan:8080\n"),
            Err(ConfigError::MissingSsid)
        );
    }

    #[test]
    fn a_missing_server_is_an_error() {
        assert_eq!(
            Config::parse("ssid = HomeNet\n"),
            Err(ConfigError::MissingServer)
        );
    }

    #[test]
    fn a_line_without_a_separator_is_an_error() {
        assert_eq!(
            Config::parse("ssid = A\nserver = s:1\nnonsense\n"),
            Err(ConfigError::MalformedLine)
        );
    }

    #[test]
    fn an_overlong_value_is_rejected_rather_than_truncated() {
        // 40 characters, over the 32-byte SSID limit.
        const TEXT: &str = "ssid = xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\nserver = s:1\n";
        assert_eq!(Config::parse(TEXT), Err(ConfigError::ValueTooLong));
    }
}
