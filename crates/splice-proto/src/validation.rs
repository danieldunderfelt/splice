use crate::{DisplayRect, Frame, InputEvent, LayoutDoc, MachineId, MachineInfo, ProtoError, Stamp};
use std::collections::HashSet;

pub const MAX_COORDINATE: i32 = 1_000_000;

fn identity(id: &MachineId) -> bool {
    !id.0.is_empty() && id.0.len() <= 256
}

fn stamp(stamp: &Stamp) -> bool {
    identity(&stamp.writer) && stamp.lamport <= i64::MAX as u64
}

fn coordinate(value: i32) -> bool {
    (-MAX_COORDINATE..=MAX_COORDINATE).contains(&value)
}

fn display(display: &DisplayRect) -> bool {
    !display.id.is_empty()
        && coordinate(display.x)
        && coordinate(display.y)
        && (1..=MAX_COORDINATE as u32).contains(&display.w)
        && (1..=MAX_COORDINATE as u32).contains(&display.h)
        && display.scale.is_finite()
        && display.scale > 0.0
}

fn machine(info: &MachineInfo) -> bool {
    let mut ids = HashSet::new();
    identity(&info.id)
        && info.build.commit.len() == 40
        && info.build.commit.bytes().all(|b| b.is_ascii_hexdigit())
        && !info.build.version.is_empty()
        && info.build.version.len() <= 64
        && semver::Version::parse(&info.build.version).is_ok()
        && info.build.protocol > 0
        && !info.build.target.is_empty()
        && info.build.target.len() <= 128
        && info.build.target.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        && info
            .displays
            .iter()
            .all(|rect| display(rect) && ids.insert(&rect.id))
}

impl LayoutDoc {
    pub fn validate(&self) -> Result<(), ProtoError> {
        if !stamp(&self.stamp)
            || !self.machines.iter().all(|(id, placement)| {
                identity(id) && coordinate(placement.offset.x) && coordinate(placement.offset.y)
            })
            || !self
                .sensitivity
                .values()
                .all(|factor| factor.is_finite() && (0.25..=4.0).contains(factor))
        {
            return Err(ProtoError::InvalidData(
                "invalid workspace geometry, identity, clock, or sensitivity",
            ));
        }
        Ok(())
    }
}

impl Frame {
    pub fn validate(&self) -> Result<(), ProtoError> {
        let valid = match self {
            Frame::Hello(hello) => machine(&hello.machine) && hello.machine.build.protocol == hello.proto_max,
            Frame::Welcome(welcome) => machine(&welcome.machine) && welcome.machine.build.protocol == welcome.proto,
            Frame::MachineUpdate(info) => machine(info) && info.build.protocol == crate::PROTO_VERSION,
            Frame::LayoutSync(doc) => return doc.validate(),
            Frame::SourceClaim { stamp: value } | Frame::ClipOffer { stamp: value, .. } => {
                stamp(value)
            }
            Frame::Enter { pos, .. } => pos.x.is_finite() && pos.y.is_finite(),
            Frame::Input {
                ev: InputEvent::Motion { dx, dy } | InputEvent::ScrollPixels { dx, dy },
                ..
            } => dx.is_finite() && dy.is_finite(),
            Frame::Input {
                ev: InputEvent::Key { code, .. },
                ..
            } => (1..=0x2ff).contains(code),
            _ => true,
        };
        if valid {
            Ok(())
        } else {
            Err(ProtoError::InvalidData(
                "invalid peer identity, display, clock, or input",
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MachinePlacement, Os, Vec2I};
    use std::collections::BTreeMap;

    #[test]
    fn build_metadata_must_be_valid_and_match_the_handshake() {
        let info = MachineInfo { build: crate::BuildInfo::current(), id: MachineId("peer".into()), hostname: "peer".into(), os: Os::Linux, displays: vec![] };
        let mut hello = crate::Hello { proto_min: crate::PROTO_VERSION, proto_max: crate::PROTO_VERSION, machine: info, caps: vec![] };
        assert!(Frame::Hello(hello.clone()).validate().is_ok());
        hello.machine.build.protocol += 1;
        assert!(Frame::Hello(hello.clone()).validate().is_err());
        hello.machine.build = crate::BuildInfo::current();
        hello.machine.build.commit = "untrusted".into();
        assert!(Frame::Hello(hello.clone()).validate().is_err());
        hello.machine.build = crate::BuildInfo::current();
        hello.machine.build.version = "invalid version".into();
        assert!(Frame::Hello(hello).validate().is_err());
    }

    #[test]
    fn rejects_nonfinite_input_and_overflowing_geometry() {
        assert!(Frame::Input {
            session: 1,
            ev: InputEvent::Motion {
                dx: f64::NAN,
                dy: 0.0
            }
        }
        .validate()
        .is_err());
        let rect = DisplayRect {
            id: "display".into(),
            x: i32::MAX,
            y: 0,
            w: 1920,
            h: 1080,
            scale: 1.25,
        };
        assert!(Frame::MachineUpdate(MachineInfo {
            build: crate::BuildInfo::current(),
            id: MachineId("peer".into()),
            hostname: "peer".into(),
            os: Os::Linux,
            displays: vec![rect]
        })
        .validate()
        .is_err());
        let mut doc = LayoutDoc {
            stamp: Stamp {
                lamport: 1,
                writer: MachineId("peer".into()),
            },
            machines: BTreeMap::from([(
                MachineId("peer".into()),
                MachinePlacement {
                    offset: Vec2I { x: -1920, y: 0 },
                    enabled: true,
                },
            )]),
            sensitivity: BTreeMap::new(),
        };
        assert!(doc.validate().is_ok());
        doc.stamp.lamport = u64::MAX;
        assert!(doc.validate().is_err());
        doc.stamp.lamport = 1;
        doc.sensitivity.insert("peer|other".into(), f64::INFINITY);
        assert!(doc.validate().is_err());
    }
}
