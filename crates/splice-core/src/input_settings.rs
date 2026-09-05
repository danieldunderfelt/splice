use serde::{Deserialize, Serialize};
use splice_proto::{raw::InputMode, MachineId};
use std::{collections::BTreeMap, path::Path};

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum CrossingPolicy {
    #[default]
    Immediate,
    Dwell {
        milliseconds: u32,
    },
    Resistance {
        points: f64,
        decay_per_second: f64,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputSettings {
    pub destinations: BTreeMap<MachineId, InputMode>,
    pub focus_lock: bool,
    pub crossing: CrossingPolicy,
}

impl InputSettings {
    pub fn mode(&self, target: &MachineId) -> InputMode {
        self.destinations
            .get(target)
            .copied()
            .unwrap_or(InputMode::Desktop)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.destinations.len() <= 1024
                && self
                    .destinations
                    .keys()
                    .all(|id| !id.0.is_empty() && id.0.len() <= 256),
            "invalid input destination settings"
        );
        match self.crossing {
            CrossingPolicy::Immediate => {}
            CrossingPolicy::Dwell { milliseconds } => anyhow::ensure!(
                (50..=5000).contains(&milliseconds),
                "edge dwell must be 50–5000 ms"
            ),
            CrossingPolicy::Resistance {
                points,
                decay_per_second,
            } => anyhow::ensure!(
                points.is_finite()
                    && (5.0..=1000.0).contains(&points)
                    && decay_per_second.is_finite()
                    && (0.0..=1000.0).contains(&decay_per_second),
                "invalid edge resistance"
            ),
        }
        Ok(())
    }

    pub fn load(dir: &Path, edge_dwell_ms: u32) -> anyhow::Result<Self> {
        match std::fs::read(dir.join("input.json")) {
            Ok(bytes) => {
                let settings: Self = serde_json::from_slice(&bytes)?;
                settings.validate()?;
                Ok(settings)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut settings = Self::default();
                if edge_dwell_ms != 0 {
                    settings.crossing = CrossingPolicy::Dwell {
                        milliseconds: edge_dwell_ms,
                    };
                }
                settings.validate()?;
                Ok(settings)
            }
            Err(error) => Err(error.into()),
        }
    }

    pub fn save(&self, dir: &Path) -> anyhow::Result<()> {
        use std::io::Write;
        self.validate()?;
        let mut file = std::fs::File::create(dir.join("input.json.tmp"))?;
        file.write_all(&serde_json::to_vec_pretty(self)?)?;
        file.sync_all()?;
        std::fs::rename(dir.join("input.json.tmp"), dir.join("input.json"))?;
        std::fs::File::open(dir)?.sync_all()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip_and_reject_bad_input_without_overwriting_it() {
        let dir = tempfile::tempdir().unwrap();
        let settings = InputSettings {
            destinations: [(MachineId("linux".into()), InputMode::Raw)].into(),
            focus_lock: true,
            crossing: CrossingPolicy::Resistance {
                points: 73.0,
                decay_per_second: 24.0,
            },
        };
        settings.save(dir.path()).unwrap();
        assert_eq!(InputSettings::load(dir.path(), 0).unwrap(), settings);
        for bytes in [
            r#"{"destinations":{},"focus_lock":false,"crossing":{"Dwell":{"milliseconds":0}}}"#,
            r#"{"destinations":{},"focus_lock":false,"crossing":{"Dwell":{"milliseconds":100,"extra":1}}}"#,
            r#"{"destinations":{},"focus_lock":false,"crossing":"Immediate","extra":1}"#,
            "invalid json",
        ] {
            std::fs::write(dir.path().join("input.json"), bytes).unwrap();
            assert!(InputSettings::load(dir.path(), 0).is_err());
            assert_eq!(
                std::fs::read_to_string(dir.path().join("input.json")).unwrap(),
                bytes
            );
        }
    }
}
