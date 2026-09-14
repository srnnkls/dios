use serde::Serialize;

use super::catalog::{GRANULE, Lane};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Method {
    Mmap,
    Explicit,
    Automatic,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub(super) struct Config {
    pub(super) lane: Lane,
    pub(super) method: Method,
    pub(super) granule: u32,
    pub(super) credits: u32,
    pub(super) read_limit: u32,
    pub(super) arena_bytes: u32,
}

impl Config {
    pub(super) fn parse(text: &str) -> Result<Self, String> {
        let parts: Vec<_> = text.split(':').collect();
        let [shape, method, granule, credits, limit] = parts.as_slice() else {
            return Err("scan config: cold|geometry|pressure:mmap|explicit|automatic:KiB:credits:read_limit".to_owned());
        };
        let (lane, arena_bytes) = match *shape {
            "cold" => (Lane::ScanDecode, 4 << 20),
            "geometry" => (Lane::ScanDecode, 8 << 20),
            "pressure" => (Lane::PressureScan, 64 << 20),
            _ => return Err("unknown scan shape".to_owned()),
        };
        let method = match *method {
            "mmap" => Method::Mmap,
            "explicit" => Method::Explicit,
            "automatic" => Method::Automatic,
            _ => return Err("unknown scan method".to_owned()),
        };
        let granule: u32 = granule.parse().map_err(super::fixture::error)?;
        if ![4, 64, 256, 1024].contains(&granule) {
            return Err("scan granule must be 4, 64, 256 or 1024 KiB".to_owned());
        }
        let value = Self {
            lane,
            method,
            granule: granule * 1024,
            arena_bytes,
            credits: credits.parse().map_err(super::fixture::error)?,
            read_limit: limit.parse().map_err(super::fixture::error)?,
        };
        value.validate()?;
        Ok(value)
    }

    fn validate(self) -> Result<(), String> {
        if self.method == Method::Mmap {
            if self.granule != GRANULE || self.credits != 0 || self.read_limit != 0 {
                return Err("mmap has 4 KiB observation pages and no Dios credits".to_owned());
            }
        } else {
            if !(2..=512).contains(&self.read_limit) {
                return Err("read limit must be 2..512".to_owned());
            }
            if self.credits == 0 || self.credits >= self.read_limit {
                return Err("credits must reserve one demand read".to_owned());
            }
            if self.frames() < 1 + 3 * self.read_limit + self.credits {
                return Err(
                    "scan arena cannot cover the reclamation watermark and credits".to_owned(),
                );
            }
        }
        assert!(self.arena_bytes.is_multiple_of(self.granule));
        assert!(
            self.lane
                .operations()
                .is_multiple_of(self.granule / GRANULE)
        );
        Ok(())
    }

    pub(super) fn frames(self) -> u32 {
        self.arena_bytes / self.granule
    }

    pub(super) fn requests(self) -> u32 {
        self.lane.operations() / (self.granule / GRANULE)
    }

    pub(super) fn requests_per_pass(self) -> u32 {
        if self.lane == Lane::PressureScan {
            self.requests() / 3
        } else {
            self.requests()
        }
    }
}

pub(super) fn catalog() -> Vec<String> {
    let mut configurations = Vec::with_capacity(32);
    for shape in ["cold", "pressure"] {
        configurations.push(format!("{shape}:mmap:4:0:0"));
        configurations.push(format!("{shape}:automatic:4:32:64"));
        for credits in [16, 32, 64] {
            for method in ["explicit", "automatic"] {
                configurations.push(format!("{shape}:{method}:4:{credits}:128"));
            }
        }
    }
    for shape in ["geometry", "pressure"] {
        for granule in [4, 64, 256, 1024] {
            let limit = 2048 / granule;
            configurations.push(format!("{shape}:explicit:{granule}:{}:{limit}", limit - 1));
        }
    }
    assert!(configurations.len() <= configurations.capacity());
    configurations
}

#[cfg(test)]
mod tests {
    #[test]
    fn sweep_preserves_accepted_bytes_and_has_valid_reclamation_capacity() {
        use super::{Config, catalog};
        for text in catalog() {
            let config = Config::parse(&text).expect("valid catalog configuration");
            if text.starts_with("geometry:") {
                assert_eq!(config.granule * config.read_limit, 2 << 20);
                assert_eq!(config.arena_bytes, 8 << 20);
                assert_eq!(config.requests() * config.granule, 64 << 20);
            }
        }
    }

    #[test]
    fn unsupported_geometry_and_unreserved_demand_are_rejected_before_io() {
        use super::Config;
        for text in [
            "cold:explicit:1024:1:2",
            "cold:explicit:4:64:64",
            "geometry:explicit:3:16:32",
            "pressure:explicit:4:1:4294967295",
            "pressure:mmap:64:0:0",
        ] {
            assert!(Config::parse(text).is_err(), "{text}");
        }
    }
}
