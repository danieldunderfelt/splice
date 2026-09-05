use anyhow::{anyhow, bail, ensure, Result};
use hidparser::{Report, ReportField};
use splice_proto::raw::{keyboard_code, RawEvent};
use std::collections::{BTreeMap, BTreeSet};

pub struct Decoder {
    reports: BTreeMap<u8, Report>,
    held: BTreeMap<u8, BTreeSet<Control>>,
    features: Vec<Report>,
    physical_values: Vec<i32>,
    multipliers: BTreeMap<(u8, u32), (Vec<hidparser::ReportCollection>, f64)>,
    loaded_features: BTreeSet<u8>,
    wheel_remainders: BTreeMap<(u8, u32), f64>,
    pub mouse: bool,
    pub keyboard: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Control {
    Key(u16),
    Button(u8),
}

impl Control {
    fn event(self, pressed: bool) -> RawEvent {
        match self {
            Self::Key(code) => RawEvent::Key { code, pressed },
            Self::Button(number) => RawEvent::Button { number, pressed },
        }
    }
}

fn control(page: u16, usage: u16) -> Result<Option<Control>> {
    match page {
        9 => {
            ensure!(
                (1..=8).contains(&usage),
                "mouse button {usage} is not supported"
            );
            Ok(Some(Control::Button(usage as u8)))
        }
        7 | 12 => {
            if usage == 0 {
                return Ok(None);
            }
            ensure!(
                page != 7 || usage > 3,
                "keyboard reported rollover or a hardware error"
            );
            let code = super::usages::key(page, usage)
                .ok_or_else(|| anyhow!("unsupported HID key {page:04x}:{usage:04x}"))?;
            ensure!(
                keyboard_code(code),
                "unsupported HID key {page:04x}:{usage:04x}"
            );
            Ok(Some(Control::Key(code)))
        }
        _ => Ok(None),
    }
}

impl Decoder {
    pub fn reset(&mut self) {
        self.held.clear();
        self.wheel_remainders.clear();
    }

    pub fn new(descriptor: &[u8]) -> Result<Self> {
        ensure!(
            !descriptor.is_empty() && descriptor.len() <= 4096,
            "invalid HID descriptor size"
        );
        let physical_values = preflight(descriptor)?;
        let parsed = hidparser::parse_report_descriptor(descriptor)
            .map_err(|e| anyhow!("invalid HID descriptor: {e:?}"))?;
        ensure!(
            parsed.bad_input_reports.is_empty(),
            "HID descriptor contains invalid input reports"
        );
        let mut mouse = false;
        let mut keyboard = false;
        let mut reports = BTreeMap::new();
        for report in parsed.input_reports {
            ensure!(
                report.size_in_bits <= 32768,
                "HID report exceeds 4096 bytes"
            );
            for field in &report.fields {
                match field {
                    ReportField::Variable(f) => {
                        if f.attributes.constant || !matches!(f.usage.page(), 1 | 7 | 9 | 12) {
                            continue;
                        }
                        ensure!(
                            (1..=32).contains(&f.bits.len()),
                            "unsupported HID field width"
                        );
                        if f.usage.page() == 1 && matches!(f.usage.id(), 0x30 | 0x31) {
                            ensure!(
                                f.attributes.relative,
                                "absolute pointing devices are not supported in raw mode"
                            );
                            mouse = true;
                        }
                        if f.usage.page() == 7 {
                            keyboard = true;
                        }
                    }
                    ReportField::Array(f) => {
                        if f.attributes.constant || !f.usage_list.iter().any(|r| matches!(r.start() >> 16, 7 | 9 | 12)) {
                            continue;
                        }
                        ensure!(
                            (1..=32).contains(&f.bits.len()),
                            "unsupported HID array width"
                        );
                        if f.usage_list.iter().any(|r| r.start() >> 16 == 7) {
                            keyboard = true;
                        }
                    }
                    ReportField::Padding(_) => {}
                }
            }
            let id = report.report_id.map(u32::from).unwrap_or(0);
            ensure!(id <= 255, "invalid HID report ID");
            ensure!(
                reports.insert(id as u8, report).is_none(),
                "duplicate HID report ID"
            );
        }
        ensure!(!reports.is_empty(), "device has no HID input reports");
        let features: Vec<_> = parsed.features.into_iter().filter(|report| report.fields.iter().any(|field| matches!(field, ReportField::Variable(f) if f.usage.page() == 1 && f.usage.id() == 0x48))).collect();
        ensure!(
            parsed.bad_features.is_empty(),
            "device contains unreadable HID feature reports"
        );
        for report in &features {
            ensure!(report.size_in_bits <= 32768, "HID feature report too large");
            for field in &report.fields {
                if let ReportField::Variable(f) = field {
                    ensure!(
                        (1..=32).contains(&f.bits.len()),
                        "invalid HID feature width"
                    );
                }
            }
        }
        Ok(Self {
            reports,
            held: BTreeMap::new(),
            features,
            physical_values,
            multipliers: BTreeMap::new(),
            loaded_features: BTreeSet::new(),
            wheel_remainders: BTreeMap::new(),
            mouse,
            keyboard,
        })
    }

    pub fn required_features(&self) -> Vec<(u8, usize)> {
        self.features
            .iter()
            .map(|r| {
                (
                    r.report_id.map(u32::from).unwrap_or(0) as u8,
                    r.size_in_bits.div_ceil(8),
                )
            })
            .collect()
    }

    pub fn feature(&mut self, id: u8, bytes: &[u8]) -> Result<()> {
        let report = self
            .features
            .iter()
            .find(|r| r.report_id.map(u32::from).unwrap_or(0) == u32::from(id))
            .ok_or_else(|| anyhow!("unknown HID feature report"))?;
        ensure!(
            bytes.len() == report.size_in_bits.div_ceil(8),
            "invalid HID feature report length"
        );
        let mut multipliers = self.multipliers.clone();
        for field in &report.fields {
            if let ReportField::Variable(f) = field {
                if f.usage.page() != 1 || f.usage.id() != 0x48 {
                    continue;
                }
                let value = f
                    .field_value(bytes)
                    .ok_or_else(|| anyhow!("invalid wheel resolution multiplier"))?
                    as f64;
                let logical_min = i32::from(f.logical_minimum) as f64;
                let logical_max = i32::from(f.logical_maximum) as f64;
                let physical_min = self
                    .physical_values
                    .iter()
                    .copied()
                    .find(|v| f.physical_minimum == Some((*v).into()))
                    .ok_or_else(|| anyhow!("wheel multiplier has no physical minimum"))?
                    as f64;
                let physical_max = self
                    .physical_values
                    .iter()
                    .copied()
                    .find(|v| f.physical_maximum == Some((*v).into()))
                    .ok_or_else(|| anyhow!("wheel multiplier has no physical maximum"))?
                    as f64;
                ensure!(
                    logical_max > logical_min,
                    "wheel multiplier range is invalid"
                );
                let multiplier = (value - logical_min) * (physical_max - physical_min)
                    / (logical_max - logical_min)
                    + physical_min;
                ensure!(
                    multiplier.is_finite() && (1.0..=65536.0).contains(&multiplier),
                    "wheel resolution multiplier is invalid"
                );
                multipliers.insert((id, f.bits.start), (f.member_of.clone(), multiplier));
            }
        }
        for (collection, multiplier) in multipliers.values() {
            ensure!(
                multipliers
                    .values()
                    .all(
                        |(other_collection, other_multiplier)| collection != other_collection
                            || multiplier == other_multiplier
                    ),
                "ambiguous HID wheel resolution collections"
            );
        }
        self.multipliers = multipliers;
        self.loaded_features.insert(id);
        Ok(())
    }

    pub fn snapshot(&self) -> Vec<RawEvent> {
        self.held
            .values()
            .flatten()
            .copied()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(|key| key.event(true))
            .collect()
    }

    pub fn decode(&mut self, id: u8, payload: &[u8]) -> Result<Vec<RawEvent>> {
        ensure!(
            self.loaded_features.len() == self.features.len(),
            "wheel resolution must be read before raw capture"
        );
        let report = self
            .reports
            .get(&id)
            .ok_or_else(|| anyhow!("unknown HID report ID {id}"))?;
        ensure!(
            payload.len() == report.size_in_bits.div_ceil(8),
            "HID report has an unexpected length"
        );
        let mut remainders = self.wheel_remainders.clone();
        let mut now = BTreeSet::new();
        let mut motion = (0i32, 0i32);
        let mut wheel = (0i32, 0i32);
        for field in &report.fields {
            match field {
                ReportField::Variable(f) if !f.attributes.constant => {
                    let page = f.usage.page();
                    let usage = f.usage.id();
                    if !matches!(page, 1 | 7 | 9 | 12) {
                        continue;
                    }
                    let value = f
                        .field_value(payload)
                        .ok_or_else(|| anyhow!("invalid HID input field"))?;
                    let axis = match (page, usage) {
                        (1, 0x30) => Some((&mut motion.0, 1)),
                        (1, 0x31) => Some((&mut motion.1, 1)),
                        (1, 0x38) => Some((&mut wheel.1, 120)),
                        (12, 0x238) => Some((&mut wheel.0, 120)),
                        _ => None,
                    };
                    if let Some((axis, scale)) = axis {
                        ensure!(
                            f.attributes.relative,
                            "raw motion and wheel must be relative"
                        );
                        let delta = if scale == 120 {
                            let multiplier = self
                                .multipliers
                                .values()
                                .filter(|(collection, _)| f.member_of.starts_with(collection))
                                .max_by_key(|(collection, _)| collection.len())
                                .map(|(_, multiplier)| *multiplier)
                                .unwrap_or(1.0);
                            let remainder = remainders.entry((id, f.bits.start)).or_default();
                            let exact = value as f64 * 120.0 / multiplier + *remainder;
                            ensure!(
                                exact >= i32::MIN as f64 && exact <= i32::MAX as f64,
                                "HID wheel overflow"
                            );
                            let delta = exact.trunc() as i32;
                            *remainder = exact - f64::from(delta);
                            delta
                        } else {
                            i32::try_from(value)?
                        };
                        *axis = axis
                            .checked_add(delta)
                            .ok_or_else(|| anyhow!("HID axis overflow"))?;
                    } else if value != 0 {
                        if let Some(key) = control(page, usage)? {
                            now.insert(key);
                        }
                    }
                }
                ReportField::Array(f) if !f.attributes.constant => {
                    if !f
                        .usage_list
                        .iter()
                        .any(|r| matches!(r.start() >> 16, 7 | 9 | 12))
                    {
                        continue;
                    }
                    let value = match f.field_value(payload) {
                        Some(value) => value,
                        None if f.attributes.null_state => continue,
                        None => bail!("invalid HID array value"),
                    };
                    let mut index = u32::try_from(value - i64::from(i32::from(f.logical_minimum)))?;
                    let mut usage = None;
                    for range in &f.usage_list {
                        let count = range
                            .end()
                            .checked_sub(range.start())
                            .and_then(|v| v.checked_add(1))
                            .ok_or_else(|| anyhow!("invalid HID usage range"))?;
                        if index < count {
                            usage = Some(range.start() + index);
                            break;
                        }
                        index -= count;
                    }
                    let usage = usage.ok_or_else(|| anyhow!("HID array index has no usage"))?;
                    if let Some(key) = control((usage >> 16) as u16, usage as u16)? {
                        now.insert(key);
                    }
                }
                _ => {}
            }
        }
        self.wheel_remainders = remainders;
        let before: BTreeSet<_> = self.held.values().flatten().copied().collect();
        self.held.insert(id, now);
        let after: BTreeSet<_> = self.held.values().flatten().copied().collect();
        let mut events = Vec::new();
        for key in before.difference(&after) {
            events.push(key.event(false));
        }
        for key in after.difference(&before) {
            events.push(key.event(true));
        }
        if motion != (0, 0) {
            events.push(RawEvent::Motion {
                x: motion.0,
                y: motion.1,
            });
        }
        if wheel != (0, 0) {
            events.push(RawEvent::Wheel {
                x120: wheel.0,
                y120: wheel.1,
            });
        }
        Ok(events)
    }
}

fn preflight(bytes: &[u8]) -> Result<Vec<i32>> {
    let mut physical = vec![0];
    let mut cursor = 0;
    let mut depth = 0u32;
    let mut count = 0u32;
    let mut stack = Vec::new();
    let mut expansion = 0u64;
    while cursor < bytes.len() {
        let tag = bytes[cursor];
        ensure!(tag != 0xfe, "long HID items are not supported");
        let len = match tag & 3 {
            3 => 4,
            n => n as usize,
        };
        ensure!(cursor + 1 + len <= bytes.len(), "truncated HID descriptor");
        let mut value = [0; 4];
        value[..len].copy_from_slice(&bytes[cursor + 1..cursor + 1 + len]);
        if matches!(tag & 0xfc, 0x34 | 0x44) {
            let signed = match len {
                1 => i32::from(value[0] as i8),
                2 => i32::from(i16::from_le_bytes([value[0], value[1]])),
                _ => i32::from_le_bytes(value),
            };
            physical.push(signed);
        }
        let value = u32::from_le_bytes(value);
        if tag & 0xfc == 0x94 {
            ensure!(value <= 2048, "HID report count too large");
            count = value;
        }
        if tag & 0xfc == 0x74 {
            ensure!(value <= 32768, "HID field width too large");
        }
        match tag & 0xfc {
            0xa0 => {
                depth += 1;
                ensure!(depth <= 16, "invalid HID collection nesting");
            }
            0xc0 => {
                depth = depth
                    .checked_sub(1)
                    .ok_or_else(|| anyhow!("unmatched HID collection end"))?;
            }
            0xa4 => {
                ensure!(stack.len() < 16, "HID global stack too deep");
                stack.push(count);
            }
            0xb4 => {
                count = stack
                    .pop()
                    .ok_or_else(|| anyhow!("unmatched HID global pop"))?;
            }
            0x80 | 0x90 | 0xb0 => {
                expansion += u64::from(count) * (1u64 << depth);
                ensure!(
                    expansion <= 131072,
                    "HID descriptor expansion exceeds its budget"
                );
            }
            _ => {}
        }
        cursor += 1 + len;
    }
    ensure!(depth == 0, "unclosed HID collection");
    Ok(physical)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MOUSE: &[u8] = &[
        5, 1, 9, 2, 0xa1, 1, 9, 1, 0xa1, 0, 5, 9, 0x19, 1, 0x29, 8, 0x15, 0, 0x25, 1, 0x95, 8,
        0x75, 1, 0x81, 2, 5, 1, 9, 0x30, 9, 0x31, 9, 0x38, 0x15, 0x81, 0x25, 0x7f, 0x75, 8, 0x95,
        3, 0x81, 6, 0xc0, 0xc0,
    ];
    const KEYBOARD: &[u8] = &[
        5, 1, 9, 6, 0xa1, 1, 5, 7, 0x19, 0xe0, 0x29, 0xe7, 0x15, 0, 0x25, 1, 0x75, 1, 0x95, 8,
        0x81, 2, 0x75, 8, 0x95, 1, 0x81, 1, 0x19, 0, 0x29, 0x65, 0x15, 0, 0x25, 0x65, 0x75, 8,
        0x95, 6, 0x81, 0, 0xc0,
    ];

    #[test]
    fn mouse_counts_buttons_and_wheel_keep_their_signs() {
        let mut d = Decoder::new(MOUSE).unwrap();
        assert!(d.mouse);
        assert_eq!(
            d.decode(0, &[0x80, 0x81, 127, 0xff]).unwrap(),
            vec![
                RawEvent::Button {
                    number: 8,
                    pressed: true
                },
                RawEvent::Motion { x: -127, y: 127 },
                RawEvent::Wheel {
                    x120: 0,
                    y120: -120
                }
            ]
        );
        assert_eq!(
            d.decode(0, &[0, 0, 0, 0]).unwrap(),
            vec![RawEvent::Button {
                number: 8,
                pressed: false
            }]
        );
        assert!(d.decode(0, &[0, 0]).is_err());
    }

    #[test]
    fn arrays_preserve_physical_keys_and_modifiers() {
        let mut d = Decoder::new(KEYBOARD).unwrap();
        assert!(d.keyboard);
        let keys = d.decode(0, &[2, 0, 4, 5, 6, 7, 8, 9]).unwrap();
        assert_eq!(keys.len(), 7);
        assert!(keys.contains(&RawEvent::Key {
            code: 42,
            pressed: true
        }));
        assert_eq!(d.snapshot().len(), 7);
        assert!(d.decode(0, &[2, 0, 1, 1, 1, 1, 1, 1]).is_err());
        assert_eq!(d.snapshot().len(), 7);
        assert_eq!(d.decode(0, &[0; 8]).unwrap().len(), 7);
    }

    #[test]
    fn numbered_wide_axes_and_nkro_reports_are_independent() {
        let descriptor = [
            5, 1, 9, 2, 0xa1, 1, 0x85, 1, 0x16, 0, 0x80, 0x26, 0xff, 0x7f, 0x75, 16, 0x95, 2, 9,
            0x30, 9, 0x31, 0x81, 6, 0xc0, 5, 1, 9, 6, 0xa1, 1, 0x85, 2, 5, 7, 0x19, 4, 0x29, 19,
            0x15, 0, 0x25, 1, 0x75, 1, 0x95, 16, 0x81, 2, 0xc0,
        ];
        let mut decoder = Decoder::new(&descriptor).unwrap();
        assert!(decoder.mouse && decoder.keyboard);
        assert_eq!(
            decoder.decode(1, &[0, 0x80, 0xff, 0x7f]).unwrap(),
            vec![RawEvent::Motion {
                x: -32768,
                y: 32767
            }]
        );
        assert_eq!(decoder.decode(2, &[0xff, 0xff]).unwrap().len(), 16);
        assert!(decoder.decode(1, &[0, 0, 0, 0]).unwrap().is_empty());
        assert_eq!(decoder.snapshot().len(), 16);
        assert_eq!(decoder.decode(2, &[0, 0]).unwrap().len(), 16);
    }

    #[test]
    fn media_keys_are_physical_key_transitions() {
        let descriptor = [
            5, 12, 9, 1, 0xa1, 1, 0x15, 0, 0x25, 1, 0x75, 1, 0x95, 1, 9, 0xe9, 0x81, 2, 0x75, 7,
            0x95, 1, 0x81, 1, 0xc0,
        ];
        let mut decoder = Decoder::new(&descriptor).unwrap();
        assert_eq!(
            decoder.decode(0, &[1]).unwrap(),
            vec![RawEvent::Key {
                code: 115,
                pressed: true
            }]
        );
        assert_eq!(
            decoder.decode(0, &[0]).unwrap(),
            vec![RawEvent::Key {
                code: 115,
                pressed: false
            }]
        );
    }

    #[test]
    fn wheel_resolution_is_read_and_fractional_counts_are_preserved() {
        let descriptor = [
            5, 1, 9, 2, 0xa1, 1, 0x85, 1, 9, 0x48, 0x15, 0, 0x25, 1, 0x35, 1, 0x45, 16, 0x75, 8,
            0x95, 1, 0xb1, 2, 9, 0x38, 0x15, 0x81, 0x25, 0x7f, 0x75, 8, 0x95, 1, 0x81, 6, 0xc0,
        ];
        let mut decoder = Decoder::new(&descriptor).unwrap();
        assert!(decoder.decode(1, &[1]).is_err());
        assert_eq!(decoder.required_features(), vec![(1, 1)]);
        decoder.feature(1, &[1]).unwrap();
        let mut sum = 0;
        for _ in 0..16 {
            for ev in decoder.decode(1, &[1]).unwrap() {
                if let RawEvent::Wheel { y120, .. } = ev {
                    sum += y120;
                }
            }
        }
        assert_eq!(sum, 120);
    }

    #[test]
    fn repeated_fields_and_nested_collection_expansion_are_bounded() {
        let mut repeated = vec![0x96, 0, 8, 0x75, 1];
        for _ in 0..100 {
            repeated.extend([0x81, 2]);
        }
        assert!(preflight(&repeated).is_err());
        let mut nested = vec![0x96, 0, 8, 0x75, 1];
        for _ in 0..16 {
            nested.extend([0xa1, 1]);
        }
        nested.extend([0x81, 2]);
        nested.extend([0xc0; 16]);
        assert!(preflight(&nested).is_err());
    }

    #[test]
    fn malformed_descriptors_are_rejected_before_expansion() {
        for d in [
            &[0x95, 0xff, 0x96, 0xff, 0xff][..],
            &[0xa1, 1],
            &[0xc0],
            &[0x77, 1],
            &[0xfe, 1, 1],
        ] {
            assert!(Decoder::new(d).is_err());
        }
    }
}
