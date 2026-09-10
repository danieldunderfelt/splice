use anyhow::{bail, ensure, Result};
use serde::{Deserialize, Serialize};
use splice_proto::{
    raw::{keyboard_code, InputMode, RawEvent, RawLedger, RawReport, MAX_DEVICES},
    InputEvent, PointerButton,
};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

const MAX_TRANSITIONS: usize = 4096;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Totals {
    Raw {
        x: i64,
        y: i64,
        wheel_x: i64,
        wheel_y: i64,
    },
    Desktop {
        x: f64,
        y: f64,
        scroll_x: f64,
        scroll_y: f64,
        wheel_x: i64,
        wheel_y: i64,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Position {
    pub serial: u64,
    pub captured_us: u64,
    pub totals: Totals,
    pub barrier: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Transition {
    pub id: u64,
    pub position: Position,
    pub event: InputEvent,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Update {
    pub position: Position,
    pub transitions: Vec<Transition>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Delivery {
    pub captured_us: u64,
    pub events: Events,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Events {
    Raw(Vec<RawEvent>),
    Desktop(Vec<InputEvent>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Held {
    Key(u16),
    Button(u8),
}

pub struct Sender {
    mode: InputMode,
    ledger: RawLedger,
    devices: BTreeMap<u64, BTreeSet<Held>>,
    position: Option<Position>,
    pending: VecDeque<Transition>,
    serial: u64,
    next_transition: u64,
}

impl Sender {
    pub fn new(mode: InputMode) -> Self {
        Self {
            mode,
            ledger: RawLedger::default(),
            devices: BTreeMap::new(),
            position: None,
            pending: VecDeque::new(),
            serial: 0,
            next_transition: 0,
        }
    }

    pub fn push_raw(&mut self, report: &RawReport) -> Result<()> {
        ensure!(
            self.mode == InputMode::Raw,
            "raw report in desktop input mode"
        );
        report.validate().map_err(anyhow::Error::msg)?;
        let serial = self
            .serial
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("position serial exhausted"))?;
        let totals = self.raw_totals_after(&report.events)?;
        let (devices, edge_count) = self.preview_raw_edges(report)?;
        ensure!(
            self.pending.len().saturating_add(edge_count) <= MAX_TRANSITIONS,
            "transition journal is full"
        );
        let edge_count = u64::try_from(edge_count)?;
        self.next_transition
            .checked_add(edge_count)
            .ok_or_else(|| anyhow::anyhow!("transition sequence exhausted"))?;

        let events = self.ledger.apply(report).map_err(anyhow::Error::msg)?;
        ensure!(
            events.iter().filter(|event| is_raw_edge(event)).count() as u64 == edge_count,
            "raw ledger state diverged"
        );

        let mut running = self.current_raw_totals();
        for event in events {
            match event {
                RawEvent::Motion { x, y } => {
                    running.0 = running.0.checked_add(i64::from(x)).unwrap();
                    running.1 = running.1.checked_add(i64::from(y)).unwrap();
                }
                RawEvent::Wheel { x120, y120 } => {
                    running.2 = running.2.checked_add(i64::from(x120)).unwrap();
                    running.3 = running.3.checked_add(i64::from(y120)).unwrap();
                }
                RawEvent::Key { code, pressed } => {
                    self.push_transition(
                        serial,
                        report.captured_us,
                        raw_totals(running),
                        InputEvent::Key {
                            code: u32::from(code),
                            pressed,
                        },
                    );
                }
                RawEvent::Button { number, pressed } => {
                    self.push_transition(
                        serial,
                        report.captured_us,
                        raw_totals(running),
                        InputEvent::Button {
                            button: raw_button(number).unwrap(),
                            pressed,
                        },
                    );
                }
                RawEvent::Removed => unreachable!(),
            }
        }
        ensure!(raw_totals(running) == totals, "raw total preview diverged");
        self.devices = devices;
        self.serial = serial;
        self.position = Some(Position {
            serial,
            captured_us: report.captured_us,
            totals,
            barrier: self.next_transition,
        });
        Ok(())
    }

    pub fn push_desktop(&mut self, event: InputEvent, captured_us: u64) -> Result<()> {
        ensure!(
            self.mode == InputMode::Desktop,
            "desktop event in raw input mode"
        );
        let serial = self
            .serial
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("position serial exhausted"))?;
        let mut totals = self.current_desktop_totals();
        let edge = match event {
            InputEvent::Motion { dx, dy } => {
                ensure!(
                    dx.is_finite() && dy.is_finite(),
                    "non-finite desktop motion"
                );
                totals.0 = finite_add(totals.0, dx)?;
                totals.1 = finite_add(totals.1, dy)?;
                None
            }
            InputEvent::ScrollPixels { dx, dy } => {
                ensure!(
                    dx.is_finite() && dy.is_finite(),
                    "non-finite desktop scroll"
                );
                totals.2 = finite_add(totals.2, dx)?;
                totals.3 = finite_add(totals.3, dy)?;
                None
            }
            InputEvent::Scroll120 { dx, dy } => {
                totals.4 = totals
                    .4
                    .checked_add(i64::from(dx))
                    .ok_or_else(|| anyhow::anyhow!("desktop horizontal wheel total overflow"))?;
                totals.5 = totals
                    .5
                    .checked_add(i64::from(dy))
                    .ok_or_else(|| anyhow::anyhow!("desktop vertical wheel total overflow"))?;
                None
            }
            InputEvent::Key { code, .. } => {
                ensure!((1..=0x2ff).contains(&code), "unsupported desktop key code");
                Some(event)
            }
            InputEvent::Button { .. } | InputEvent::ScrollStop { .. } => Some(event),
        };
        if let Some(event) = edge {
            ensure!(
                self.pending.len() < MAX_TRANSITIONS,
                "transition journal is full"
            );
            self.next_transition
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("transition sequence exhausted"))?;
            self.push_transition(serial, captured_us, desktop_totals(totals), event);
        }
        self.serial = serial;
        self.position = Some(Position {
            serial,
            captured_us,
            totals: desktop_totals(totals),
            barrier: self.next_transition,
        });
        Ok(())
    }

    pub fn position(&self) -> Option<&Position> {
        self.position.as_ref()
    }

    pub fn pending(&self) -> &VecDeque<Transition> {
        &self.pending
    }

    pub fn acknowledge(&mut self, next: u64) -> Result<()> {
        ensure!(
            next <= self.next_transition,
            "acknowledgement exceeds issued transition sequence"
        );
        while self.pending.front().is_some_and(|item| item.id < next) {
            self.pending.pop_front();
        }
        Ok(())
    }

    fn current_raw_totals(&self) -> (i64, i64, i64, i64) {
        match self.position.as_ref().map(|position| &position.totals) {
            Some(Totals::Raw {
                x,
                y,
                wheel_x,
                wheel_y,
            }) => (*x, *y, *wheel_x, *wheel_y),
            None => (0, 0, 0, 0),
            Some(Totals::Desktop { .. }) => unreachable!(),
        }
    }

    fn current_desktop_totals(&self) -> (f64, f64, f64, f64, i64, i64) {
        match self.position.as_ref().map(|position| &position.totals) {
            Some(Totals::Desktop {
                x,
                y,
                scroll_x,
                scroll_y,
                wheel_x,
                wheel_y,
            }) => (*x, *y, *scroll_x, *scroll_y, *wheel_x, *wheel_y),
            None => (0.0, 0.0, 0.0, 0.0, 0, 0),
            Some(Totals::Raw { .. }) => unreachable!(),
        }
    }

    fn raw_totals_after(&self, events: &[RawEvent]) -> Result<Totals> {
        let mut totals = self.current_raw_totals();
        for event in events {
            match *event {
                RawEvent::Motion { x, y } => {
                    totals.0 = totals
                        .0
                        .checked_add(i64::from(x))
                        .ok_or_else(|| anyhow::anyhow!("raw x total overflow"))?;
                    totals.1 = totals
                        .1
                        .checked_add(i64::from(y))
                        .ok_or_else(|| anyhow::anyhow!("raw y total overflow"))?;
                }
                RawEvent::Wheel { x120, y120 } => {
                    totals.2 = totals
                        .2
                        .checked_add(i64::from(x120))
                        .ok_or_else(|| anyhow::anyhow!("raw horizontal wheel total overflow"))?;
                    totals.3 = totals
                        .3
                        .checked_add(i64::from(y120))
                        .ok_or_else(|| anyhow::anyhow!("raw vertical wheel total overflow"))?;
                }
                RawEvent::Key { .. } | RawEvent::Button { .. } | RawEvent::Removed => {}
            }
        }
        Ok(raw_totals(totals))
    }

    fn preview_raw_edges(
        &self,
        report: &RawReport,
    ) -> Result<(BTreeMap<u64, BTreeSet<Held>>, usize)> {
        let mut devices = self.devices.clone();
        if !devices.contains_key(&report.device) && devices.len() == MAX_DEVICES {
            bail!("too many raw input devices");
        }
        if report.events != [RawEvent::Removed] {
            devices.entry(report.device).or_default();
        }
        let mut edges = 0usize;
        for event in &report.events {
            let (held, pressed) = match *event {
                RawEvent::Key { code, pressed } => (Held::Key(code), pressed),
                RawEvent::Button { number, pressed } => (Held::Button(number), pressed),
                RawEvent::Removed => {
                    if let Some(removed) = devices.remove(&report.device) {
                        for held in removed {
                            if !devices.values().any(|items| items.contains(&held)) {
                                edges += 1;
                            }
                        }
                    }
                    continue;
                }
                RawEvent::Motion { .. } | RawEvent::Wheel { .. } => continue,
            };
            let was_held = devices.values().any(|items| items.contains(&held));
            let device = devices.entry(report.device).or_default();
            if pressed {
                device.insert(held);
            } else {
                device.remove(&held);
            }
            let is_held = devices.values().any(|items| items.contains(&held));
            if was_held != is_held {
                edges += 1;
            }
        }
        Ok((devices, edges))
    }

    fn push_transition(
        &mut self,
        serial: u64,
        captured_us: u64,
        totals: Totals,
        event: InputEvent,
    ) {
        let id = self.next_transition;
        self.pending.push_back(Transition {
            id,
            position: Position {
                serial,
                captured_us,
                totals,
                barrier: id,
            },
            event,
        });
        self.next_transition += 1;
    }
}

pub struct Receiver {
    mode: InputMode,
    next_transition: u64,
    buffered: BTreeMap<u64, Transition>,
    emitted: VecDeque<Transition>,
    latest_position: Option<Position>,
    applied_serial: u64,
    applied_barrier: u64,
    totals: Totals,
}

impl Receiver {
    pub fn new(mode: InputMode) -> Self {
        let totals = match mode {
            InputMode::Raw => raw_totals((0, 0, 0, 0)),
            InputMode::Desktop => desktop_totals((0.0, 0.0, 0.0, 0.0, 0, 0)),
        };
        Self {
            mode,
            next_transition: 0,
            buffered: BTreeMap::new(),
            emitted: VecDeque::new(),
            latest_position: None,
            applied_serial: 0,
            applied_barrier: 0,
            totals,
        }
    }

    pub fn receive(&mut self, update: Update) -> Result<Vec<Delivery>> {
        self.validate_update(&update)?;
        self.check_position(&update.position)?;
        let mut incoming = BTreeMap::new();
        for transition in update.transitions {
            if transition.id < self.next_transition {
                if let Some(existing) = self
                    .emitted
                    .iter()
                    .find(|existing| existing.id == transition.id)
                {
                    ensure!(existing == &transition, "conflicting duplicate transition");
                }
                continue;
            }
            match self.buffered.get(&transition.id) {
                Some(existing) => {
                    ensure!(existing == &transition, "conflicting duplicate transition")
                }
                None => match incoming.get(&transition.id) {
                    Some(existing) => {
                        ensure!(existing == &transition, "conflicting duplicate transition")
                    }
                    None => {
                        incoming.insert(transition.id, transition);
                    }
                },
            }
        }
        ensure!(
            self.buffered.len().saturating_add(incoming.len()) <= MAX_TRANSITIONS,
            "transition receive window is full"
        );
        self.buffered.extend(incoming);
        self.remember_position(update.position)?;

        let mut deliveries = Vec::new();
        while let Some(transition) = self.buffered.remove(&self.next_transition) {
            self.apply_position(&transition.position, &mut deliveries)?;
            self.push_input(
                transition.position.serial,
                transition.position.captured_us,
                transition.event,
                &mut deliveries,
            )?;
            self.next_transition = self
                .next_transition
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("transition sequence exhausted"))?;
            self.applied_barrier = self.next_transition;
            self.emitted.push_back(transition);
            if self.emitted.len() > MAX_TRANSITIONS {
                self.emitted.pop_front();
            }
        }
        if let Some(position) = self.latest_position.clone() {
            if position.barrier <= self.next_transition
                && (position.serial > self.applied_serial
                    || position.serial == self.applied_serial
                        && position.barrier >= self.applied_barrier)
            {
                self.apply_position(&position, &mut deliveries)?;
            }
        }
        Ok(deliveries
            .into_iter()
            .map(|tagged: TaggedDelivery| tagged.delivery)
            .collect())
    }

    pub fn next_transition(&self) -> u64 {
        self.next_transition
    }

    fn validate_update(&self, update: &Update) -> Result<()> {
        ensure!(
            update.transitions.len() <= MAX_TRANSITIONS,
            "too many transitions in update"
        );
        validate_position(self.mode, &update.position)?;
        ensure!(
            update.position.serial > 0,
            "position serial must be nonzero"
        );
        if update.position.barrier >= self.next_transition {
            ensure!(
                update.position.barrier - self.next_transition <= MAX_TRANSITIONS as u64,
                "position exceeds transition receive window"
            );
        }
        for transition in &update.transitions {
            validate_position(self.mode, &transition.position)?;
            validate_transition_event(self.mode, &transition.event)?;
            ensure!(
                transition.position.barrier == transition.id,
                "transition position has the wrong barrier"
            );
            ensure!(
                transition.id < update.position.barrier,
                "transition was not issued by update position"
            );
            ensure!(
                transition.position.serial <= update.position.serial,
                "transition is newer than update position"
            );
            if transition.id >= self.next_transition {
                ensure!(
                    transition.id - self.next_transition < MAX_TRANSITIONS as u64,
                    "transition exceeds receive window"
                );
            }
        }
        Ok(())
    }

    fn remember_position(&mut self, position: Position) -> Result<()> {
        self.check_position(&position)?;
        if position.serial < self.applied_serial {
            return Ok(());
        }
        match &self.latest_position {
            Some(latest) if position.serial < latest.serial => {}
            Some(latest) if position.serial == latest.serial => {
                ensure!(latest == &position, "conflicting position serial")
            }
            _ => self.latest_position = Some(position),
        }
        Ok(())
    }

    fn check_position(&self, position: &Position) -> Result<()> {
        if position.serial > self.applied_serial {
            ensure!(
                position.barrier >= self.applied_barrier,
                "source barrier rewound"
            );
        }
        if let Some(latest) = &self.latest_position {
            if position.serial == latest.serial {
                ensure!(latest == position, "conflicting position serial");
            }
        }
        Ok(())
    }

    fn apply_position(
        &mut self,
        position: &Position,
        deliveries: &mut Vec<TaggedDelivery>,
    ) -> Result<()> {
        ensure!(
            position.serial >= self.applied_serial,
            "source position serial rewound"
        );
        ensure!(
            position.barrier >= self.applied_barrier,
            "source position barrier rewound"
        );
        match (&self.totals, &position.totals) {
            (
                Totals::Raw {
                    x,
                    y,
                    wheel_x,
                    wheel_y,
                },
                Totals::Raw {
                    x: target_x,
                    y: target_y,
                    wheel_x: target_wheel_x,
                    wheel_y: target_wheel_y,
                },
            ) => {
                let motion_x = i128::from(*target_x) - i128::from(*x);
                let motion_y = i128::from(*target_y) - i128::from(*y);
                let wheel_x = i128::from(*target_wheel_x) - i128::from(*wheel_x);
                let wheel_y = i128::from(*target_wheel_y) - i128::from(*wheel_y);
                push_raw_delta(position, motion_x, motion_y, wheel_x, wheel_y, deliveries)?;
            }
            (
                Totals::Desktop {
                    x,
                    y,
                    scroll_x,
                    scroll_y,
                    wheel_x,
                    wheel_y,
                },
                Totals::Desktop {
                    x: target_x,
                    y: target_y,
                    scroll_x: target_scroll_x,
                    scroll_y: target_scroll_y,
                    wheel_x: target_wheel_x,
                    wheel_y: target_wheel_y,
                },
            ) => {
                let dx = finite_delta(*target_x, *x)?;
                let dy = finite_delta(*target_y, *y)?;
                let scroll_dx = finite_delta(*target_scroll_x, *scroll_x)?;
                let scroll_dy = finite_delta(*target_scroll_y, *scroll_y)?;
                let wheel_dx = i128::from(*target_wheel_x) - i128::from(*wheel_x);
                let wheel_dy = i128::from(*target_wheel_y) - i128::from(*wheel_y);
                if dx != 0.0 || dy != 0.0 {
                    push_tagged(
                        deliveries,
                        self.mode,
                        position.serial,
                        position.captured_us,
                        InputEvent::Motion { dx, dy },
                    )?;
                }
                if scroll_dx != 0.0 || scroll_dy != 0.0 {
                    push_tagged(
                        deliveries,
                        self.mode,
                        position.serial,
                        position.captured_us,
                        InputEvent::ScrollPixels {
                            dx: scroll_dx,
                            dy: scroll_dy,
                        },
                    )?;
                }
                push_desktop_wheel_delta(position, wheel_dx, wheel_dy, deliveries)?;
            }
            _ => bail!("position totals do not match receiver input mode"),
        }
        self.totals = position.totals.clone();
        self.applied_serial = position.serial;
        self.applied_barrier = position.barrier;
        Ok(())
    }

    fn push_input(
        &self,
        serial: u64,
        captured_us: u64,
        event: InputEvent,
        deliveries: &mut Vec<TaggedDelivery>,
    ) -> Result<()> {
        push_tagged(deliveries, self.mode, serial, captured_us, event)
    }
}

struct TaggedDelivery {
    serial: u64,
    delivery: Delivery,
}

fn raw_totals(values: (i64, i64, i64, i64)) -> Totals {
    Totals::Raw {
        x: values.0,
        y: values.1,
        wheel_x: values.2,
        wheel_y: values.3,
    }
}

fn desktop_totals(values: (f64, f64, f64, f64, i64, i64)) -> Totals {
    Totals::Desktop {
        x: values.0,
        y: values.1,
        scroll_x: values.2,
        scroll_y: values.3,
        wheel_x: values.4,
        wheel_y: values.5,
    }
}

fn finite_add(left: f64, right: f64) -> Result<f64> {
    let sum = left + right;
    ensure!(sum.is_finite(), "desktop cumulative total overflow");
    Ok(sum)
}

fn finite_delta(target: f64, current: f64) -> Result<f64> {
    let delta = target - current;
    ensure!(delta.is_finite(), "desktop cumulative delta overflow");
    Ok(delta)
}

fn is_raw_edge(event: &RawEvent) -> bool {
    matches!(event, RawEvent::Key { .. } | RawEvent::Button { .. })
}

fn raw_button(number: u8) -> Option<PointerButton> {
    match number {
        1 => Some(PointerButton::Left),
        2 => Some(PointerButton::Right),
        3 => Some(PointerButton::Middle),
        4 => Some(PointerButton::Back),
        5 => Some(PointerButton::Forward),
        6..=8 => Some(PointerButton::Other(number - 1)),
        _ => None,
    }
}

fn raw_button_number(button: PointerButton) -> Option<u8> {
    match button {
        PointerButton::Left => Some(1),
        PointerButton::Right => Some(2),
        PointerButton::Middle => Some(3),
        PointerButton::Back => Some(4),
        PointerButton::Forward => Some(5),
        PointerButton::Other(number @ 5..=7) => Some(number + 1),
        PointerButton::Other(_) => None,
    }
}

fn validate_position(mode: InputMode, position: &Position) -> Result<()> {
    ensure!(position.serial > 0, "position serial must be nonzero");
    match (&position.totals, mode) {
        (Totals::Raw { .. }, InputMode::Raw) => Ok(()),
        (
            Totals::Desktop {
                x,
                y,
                scroll_x,
                scroll_y,
                ..
            },
            InputMode::Desktop,
        ) => {
            ensure!(
                x.is_finite() && y.is_finite() && scroll_x.is_finite() && scroll_y.is_finite(),
                "non-finite desktop cumulative position"
            );
            Ok(())
        }
        _ => bail!("position totals do not match input mode"),
    }
}

fn validate_transition_event(mode: InputMode, event: &InputEvent) -> Result<()> {
    match *event {
        InputEvent::Key { code, .. } => match mode {
            InputMode::Raw => {
                let code = u16::try_from(code).map_err(|_| anyhow::anyhow!("invalid raw key"))?;
                ensure!(keyboard_code(code), "invalid raw key")
            }
            InputMode::Desktop => {
                ensure!((1..=0x2ff).contains(&code), "invalid desktop key")
            }
        },
        InputEvent::Button { button, .. } => {
            if mode == InputMode::Raw {
                ensure!(raw_button_number(button).is_some(), "invalid raw button")
            }
        }
        InputEvent::ScrollStop { .. } if mode == InputMode::Desktop => {}
        InputEvent::Motion { .. }
        | InputEvent::ScrollPixels { .. }
        | InputEvent::Scroll120 { .. }
        | InputEvent::ScrollStop { .. } => {
            bail!("transition is not a reliable desktop event")
        }
    }
    Ok(())
}

fn push_raw_delta(
    position: &Position,
    motion_x: i128,
    motion_y: i128,
    wheel_x: i128,
    wheel_y: i128,
    deliveries: &mut Vec<TaggedDelivery>,
) -> Result<()> {
    let motion_count = piece_count(motion_x).max(piece_count(motion_y));
    let wheel_count = piece_count(wheel_x).max(piece_count(wheel_y));
    let additional = motion_count
        .checked_add(wheel_count)
        .ok_or_else(|| anyhow::anyhow!("raw cumulative delta is too large"))?;
    ensure!(additional <= MAX_TRANSITIONS, "raw cumulative delta exceeds the delivery limit");
    deliveries
        .try_reserve(additional)
        .map_err(|_| anyhow::anyhow!("raw cumulative delta is too large"))?;
    let mut motion = (motion_x, motion_y);
    while motion.0 != 0 || motion.1 != 0 {
        let x = take_i32(&mut motion.0);
        let y = take_i32(&mut motion.1);
        push_raw_event(position, RawEvent::Motion { x, y }, deliveries);
    }
    let mut wheel = (wheel_x, wheel_y);
    while wheel.0 != 0 || wheel.1 != 0 {
        let x120 = take_i32(&mut wheel.0);
        let y120 = take_i32(&mut wheel.1);
        push_raw_event(position, RawEvent::Wheel { x120, y120 }, deliveries);
    }
    Ok(())
}

fn push_desktop_wheel_delta(
    position: &Position,
    wheel_x: i128,
    wheel_y: i128,
    deliveries: &mut Vec<TaggedDelivery>,
) -> Result<()> {
    let additional = piece_count(wheel_x).max(piece_count(wheel_y));
    ensure!(additional <= MAX_TRANSITIONS, "desktop cumulative wheel delta exceeds the delivery limit");
    deliveries
        .try_reserve(additional)
        .map_err(|_| anyhow::anyhow!("desktop wheel cumulative delta is too large"))?;
    let mut wheel = (wheel_x, wheel_y);
    while wheel.0 != 0 || wheel.1 != 0 {
        let dx = take_i32(&mut wheel.0);
        let dy = take_i32(&mut wheel.1);
        push_tagged(
            deliveries,
            InputMode::Desktop,
            position.serial,
            position.captured_us,
            InputEvent::Scroll120 { dx, dy },
        )?;
    }
    Ok(())
}

fn piece_count(value: i128) -> usize {
    let divisor = if value < 0 {
        -(i128::from(i32::MIN))
    } else {
        i128::from(i32::MAX)
    };
    usize::try_from((value.abs() + divisor - 1) / divisor).unwrap_or(usize::MAX)
}

fn take_i32(value: &mut i128) -> i32 {
    let piece = (*value).clamp(i128::from(i32::MIN), i128::from(i32::MAX));
    *value -= piece;
    piece as i32
}

fn push_raw_event(position: &Position, event: RawEvent, deliveries: &mut Vec<TaggedDelivery>) {
    if let Some(last) = deliveries.last_mut() {
        if last.serial == position.serial {
            let Events::Raw(events) = &mut last.delivery.events else {
                unreachable!()
            };
            events.push(event);
            return;
        }
    }
    deliveries.push(TaggedDelivery {
        serial: position.serial,
        delivery: Delivery {
            captured_us: position.captured_us,
            events: Events::Raw(vec![event]),
        },
    });
}

fn push_tagged(
    deliveries: &mut Vec<TaggedDelivery>,
    mode: InputMode,
    serial: u64,
    captured_us: u64,
    event: InputEvent,
) -> Result<()> {
    match mode {
        InputMode::Raw => {
            let event = match event {
                InputEvent::Key { code, pressed } => RawEvent::Key {
                    code: u16::try_from(code)?,
                    pressed,
                },
                InputEvent::Button { button, pressed } => RawEvent::Button {
                    number: raw_button_number(button)
                        .ok_or_else(|| anyhow::anyhow!("invalid raw button"))?,
                    pressed,
                },
                _ => bail!("raw transition is not a key or button edge"),
            };
            if let Some(last) = deliveries.last_mut() {
                if last.serial == serial {
                    let Events::Raw(events) = &mut last.delivery.events else {
                        unreachable!()
                    };
                    events.push(event);
                    return Ok(());
                }
            }
            deliveries.push(TaggedDelivery {
                serial,
                delivery: Delivery {
                    captured_us,
                    events: Events::Raw(vec![event]),
                },
            });
        }
        InputMode::Desktop => {
            if let Some(last) = deliveries.last_mut() {
                if last.serial == serial {
                    let Events::Desktop(events) = &mut last.delivery.events else {
                        unreachable!()
                    };
                    events.push(event);
                    return Ok(());
                }
            }
            deliveries.push(TaggedDelivery {
                serial,
                delivery: Delivery {
                    captured_us,
                    events: Events::Desktop(vec![event]),
                },
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(sequence: u64, captured_us: u64, events: Vec<RawEvent>) -> RawReport {
        RawReport {
            device: 1,
            sequence,
            captured_us,
            events,
        }
    }

    fn update(sender: &Sender) -> Update {
        Update {
            position: sender.position().unwrap().clone(),
            transitions: sender.pending().iter().cloned().collect(),
        }
    }

    fn raw_events(deliveries: &[Delivery]) -> Vec<RawEvent> {
        deliveries
            .iter()
            .flat_map(|delivery| match &delivery.events {
                Events::Raw(events) => events.clone(),
                Events::Desktop(_) => panic!(),
            })
            .collect()
    }

    fn desktop_events(deliveries: &[Delivery]) -> Vec<InputEvent> {
        deliveries
            .iter()
            .flat_map(|delivery| match &delivery.events {
                Events::Desktop(events) => events.clone(),
                Events::Raw(_) => panic!(),
            })
            .collect()
    }

    #[test]
    fn raw_motion_keeps_counts_and_native_report_cadence() {
        let mut sender = Sender::new(InputMode::Raw);
        let mut receiver = Receiver::new(InputMode::Raw);
        let mut deliveries = Vec::new();
        for sequence in 0..3 {
            sender
                .push_raw(&report(
                    sequence,
                    100 + sequence,
                    vec![
                        RawEvent::Motion {
                            x: 10 + sequence as i32,
                            y: -4,
                        },
                        RawEvent::Wheel { x120: 2, y120: -3 },
                    ],
                ))
                .unwrap();
            deliveries.extend(receiver.receive(update(&sender)).unwrap());
        }
        assert_eq!(deliveries.len(), 3);
        assert_eq!(
            deliveries
                .iter()
                .map(|delivery| delivery.captured_us)
                .collect::<Vec<_>>(),
            vec![100, 101, 102]
        );
        assert_eq!(
            raw_events(&deliveries),
            vec![
                RawEvent::Motion { x: 10, y: -4 },
                RawEvent::Wheel { x120: 2, y120: -3 },
                RawEvent::Motion { x: 11, y: -4 },
                RawEvent::Wheel { x120: 2, y120: -3 },
                RawEvent::Motion { x: 12, y: -4 },
                RawEvent::Wheel { x120: 2, y120: -3 }
            ]
        );
    }

    #[test]
    fn lost_reordered_and_duplicated_motion_recovers_from_totals() {
        let mut sender = Sender::new(InputMode::Raw);
        let mut updates = Vec::new();
        for sequence in 0..5 {
            sender
                .push_raw(&report(
                    sequence,
                    1000 + sequence,
                    vec![RawEvent::Motion { x: 7, y: -11 }],
                ))
                .unwrap();
            updates.push(update(&sender));
        }
        let mut receiver = Receiver::new(InputMode::Raw);
        let newest = receiver.receive(updates[4].clone()).unwrap();
        assert_eq!(
            raw_events(&newest),
            vec![RawEvent::Motion { x: 35, y: -55 }]
        );
        assert!(receiver.receive(updates[1].clone()).unwrap().is_empty());
        assert!(receiver.receive(updates[4].clone()).unwrap().is_empty());
    }

    #[test]
    fn fast_taps_survive_lost_motion_packets_and_duplicate_journal_chunks() {
        let mut sender = Sender::new(InputMode::Raw);
        sender
            .push_raw(&report(0, 10, vec![RawEvent::Motion { x: 5, y: 0 }]))
            .unwrap();
        for sequence in 1..=20 {
            sender
                .push_raw(&report(
                    sequence,
                    10 + sequence,
                    vec![RawEvent::Key {
                        code: 30,
                        pressed: sequence % 2 == 1,
                    }],
                ))
                .unwrap();
        }
        sender
            .push_raw(&report(21, 40, vec![RawEvent::Motion { x: 9, y: 0 }]))
            .unwrap();
        let full = update(&sender);
        let mut receiver = Receiver::new(InputMode::Raw);
        let first = receiver.receive(full.clone()).unwrap();
        assert_eq!(
            raw_events(&first)
                .iter()
                .filter(|event| matches!(event, RawEvent::Key { .. }))
                .count(),
            20
        );
        assert_eq!(
            raw_events(&first)
                .iter()
                .filter_map(|event| match event {
                    RawEvent::Motion { x, .. } => Some(*x),
                    _ => None,
                })
                .sum::<i32>(),
            14
        );
        assert!(receiver.receive(full).unwrap().is_empty());
    }

    #[test]
    fn missing_edge_barrier_holds_motion_until_edge_arrives() {
        let mut sender = Sender::new(InputMode::Raw);
        sender
            .push_raw(&report(
                0,
                10,
                vec![
                    RawEvent::Motion { x: 4, y: 0 },
                    RawEvent::Button {
                        number: 1,
                        pressed: true,
                    },
                ],
            ))
            .unwrap();
        sender
            .push_raw(&report(1, 20, vec![RawEvent::Motion { x: 8, y: 0 }]))
            .unwrap();
        let mut blocked = update(&sender);
        let edge = blocked.transitions.remove(0);
        let mut receiver = Receiver::new(InputMode::Raw);
        assert!(receiver.receive(blocked).unwrap().is_empty());
        let recovered = receiver
            .receive(Update {
                position: sender.position().unwrap().clone(),
                transitions: vec![edge],
            })
            .unwrap();
        assert_eq!(
            raw_events(&recovered),
            vec![
                RawEvent::Motion { x: 4, y: 0 },
                RawEvent::Button {
                    number: 1,
                    pressed: true
                },
                RawEvent::Motion { x: 8, y: 0 }
            ]
        );
        assert_eq!(receiver.next_transition(), 1);
    }

    #[test]
    fn reordered_edges_buffer_until_the_gap_is_filled() {
        let mut sender = Sender::new(InputMode::Raw);
        for (sequence, number) in [(0, 1), (1, 2)] {
            sender
                .push_raw(&report(
                    sequence,
                    50 + sequence,
                    vec![RawEvent::Button {
                        number,
                        pressed: true,
                    }],
                ))
                .unwrap();
        }
        let position = sender.position().unwrap().clone();
        let first = sender.pending()[0].clone();
        let second = sender.pending()[1].clone();
        let mut receiver = Receiver::new(InputMode::Raw);
        assert!(receiver
            .receive(Update {
                position: position.clone(),
                transitions: vec![second],
            })
            .unwrap()
            .is_empty());
        assert_eq!(
            raw_events(
                &receiver
                    .receive(Update {
                        position,
                        transitions: vec![first],
                    })
                    .unwrap()
            ),
            vec![
                RawEvent::Button {
                    number: 1,
                    pressed: true
                },
                RawEvent::Button {
                    number: 2,
                    pressed: true
                }
            ]
        );
    }

    #[test]
    fn multiple_edges_in_one_report_keep_event_and_motion_order() {
        let mut sender = Sender::new(InputMode::Raw);
        sender
            .push_raw(&report(
                0,
                77,
                vec![
                    RawEvent::Motion { x: 2, y: 3 },
                    RawEvent::Key {
                        code: 30,
                        pressed: true,
                    },
                    RawEvent::Motion { x: 5, y: 7 },
                    RawEvent::Button {
                        number: 2,
                        pressed: true,
                    },
                    RawEvent::Key {
                        code: 30,
                        pressed: false,
                    },
                ],
            ))
            .unwrap();
        let mut receiver = Receiver::new(InputMode::Raw);
        let deliveries = receiver.receive(update(&sender)).unwrap();
        assert_eq!(deliveries.len(), 1);
        assert_eq!(deliveries[0].captured_us, 77);
        assert_eq!(
            raw_events(&deliveries),
            vec![
                RawEvent::Motion { x: 2, y: 3 },
                RawEvent::Key {
                    code: 30,
                    pressed: true
                },
                RawEvent::Motion { x: 5, y: 7 },
                RawEvent::Button {
                    number: 2,
                    pressed: true
                },
                RawEvent::Key {
                    code: 30,
                    pressed: false
                }
            ]
        );
    }

    #[test]
    fn device_union_and_removal_emit_only_global_edges() {
        let mut sender = Sender::new(InputMode::Raw);
        sender
            .push_raw(&report(
                0,
                1,
                vec![RawEvent::Key {
                    code: 42,
                    pressed: true,
                }],
            ))
            .unwrap();
        let mut second = report(
            1,
            2,
            vec![RawEvent::Key {
                code: 42,
                pressed: true,
            }],
        );
        second.device = 2;
        sender.push_raw(&second).unwrap();
        let mut remove_first = report(2, 3, vec![RawEvent::Removed]);
        remove_first.device = 1;
        sender.push_raw(&remove_first).unwrap();
        let mut remove_second = report(3, 4, vec![RawEvent::Removed]);
        remove_second.device = 2;
        sender.push_raw(&remove_second).unwrap();
        assert_eq!(sender.pending().len(), 2);
        assert_eq!(
            sender
                .pending()
                .iter()
                .map(|transition| transition.event)
                .collect::<Vec<_>>(),
            vec![
                InputEvent::Key {
                    code: 42,
                    pressed: true
                },
                InputEvent::Key {
                    code: 42,
                    pressed: false
                }
            ]
        );
    }

    #[test]
    fn old_ack_is_harmless_and_future_ack_is_rejected() {
        let mut sender = Sender::new(InputMode::Desktop);
        for pressed in [true, false, true] {
            sender
                .push_desktop(InputEvent::Key { code: 30, pressed }, 1)
                .unwrap();
        }
        sender.acknowledge(2).unwrap();
        assert_eq!(sender.pending().front().unwrap().id, 2);
        sender.acknowledge(1).unwrap();
        assert_eq!(sender.pending().front().unwrap().id, 2);
        assert!(sender.acknowledge(4).is_err());
        assert_eq!(sender.pending().front().unwrap().id, 2);
    }

    #[test]
    fn pending_and_receive_windows_are_bounded() {
        let mut sender = Sender::new(InputMode::Desktop);
        for index in 0..MAX_TRANSITIONS {
            sender
                .push_desktop(
                    InputEvent::Key {
                        code: 30,
                        pressed: index % 2 == 0,
                    },
                    index as u64,
                )
                .unwrap();
        }
        assert!(sender
            .push_desktop(
                InputEvent::Key {
                    code: 30,
                    pressed: true
                },
                9
            )
            .is_err());
        sender
            .push_desktop(InputEvent::Motion { dx: 1.0, dy: 2.0 }, 10)
            .unwrap();
        let mut oversized = update(&sender);
        oversized.position.barrier += 1;
        let mut receiver = Receiver::new(InputMode::Desktop);
        assert!(receiver.receive(oversized).is_err());
    }

    #[test]
    fn checked_totals_and_serials_reject_overflow_without_advancing() {
        let mut sender = Sender::new(InputMode::Raw);
        sender.position = Some(Position {
            serial: 4,
            captured_us: 1,
            totals: Totals::Raw {
                x: i64::MAX,
                y: 0,
                wheel_x: 0,
                wheel_y: 0,
            },
            barrier: 0,
        });
        sender.serial = 4;
        assert!(sender
            .push_raw(&report(0, 2, vec![RawEvent::Motion { x: 1, y: 0 }]))
            .is_err());
        assert_eq!(sender.serial, 4);
        sender.serial = u64::MAX;
        assert!(sender
            .push_raw(&report(0, 2, vec![RawEvent::Motion { x: 0, y: 1 }]))
            .is_err());

        let mut desktop = Sender::new(InputMode::Desktop);
        desktop.next_transition = u64::MAX;
        assert!(desktop
            .push_desktop(
                InputEvent::Key {
                    code: 30,
                    pressed: true
                },
                3
            )
            .is_err());
        assert!(desktop.position().is_none());
    }

    #[test]
    fn desktop_fractional_motion_scroll_and_buttons_round_trip() {
        let mut sender = Sender::new(InputMode::Desktop);
        let events = [
            InputEvent::Motion { dx: 0.25, dy: -1.5 },
            InputEvent::ScrollPixels { dx: 1.75, dy: 2.5 },
            InputEvent::Button {
                button: PointerButton::Other(17),
                pressed: true,
            },
            InputEvent::Motion { dx: 3.5, dy: 0.5 },
        ];
        for (index, event) in events.into_iter().enumerate() {
            sender.push_desktop(event, 100 + index as u64).unwrap();
        }
        let mut receiver = Receiver::new(InputMode::Desktop);
        let deliveries = receiver.receive(update(&sender)).unwrap();
        assert_eq!(
            deliveries,
            vec![
                Delivery {
                    captured_us: 102,
                    events: Events::Desktop(vec![
                        InputEvent::Motion { dx: 0.25, dy: -1.5 },
                        InputEvent::ScrollPixels { dx: 1.75, dy: 2.5 },
                        InputEvent::Button {
                            button: PointerButton::Other(17),
                            pressed: true
                        }
                    ])
                },
                Delivery {
                    captured_us: 103,
                    events: Events::Desktop(vec![InputEvent::Motion { dx: 3.5, dy: 0.5 }])
                }
            ]
        );
    }

    #[test]
    fn desktop_discrete_wheel_recovers_from_lost_and_reordered_updates() {
        let mut sender = Sender::new(InputMode::Desktop);
        sender
            .push_desktop(InputEvent::Scroll120 { dx: 30, dy: -120 }, 10)
            .unwrap();
        let old = update(&sender);
        sender
            .push_desktop(InputEvent::Scroll120 { dx: 90, dy: -240 }, 20)
            .unwrap();
        let latest = update(&sender);
        assert_eq!(
            sender.position().unwrap().totals,
            Totals::Desktop {
                x: 0.0,
                y: 0.0,
                scroll_x: 0.0,
                scroll_y: 0.0,
                wheel_x: 120,
                wheel_y: -360,
            }
        );

        let mut receiver = Receiver::new(InputMode::Desktop);
        assert_eq!(
            receiver.receive(latest.clone()).unwrap(),
            vec![Delivery {
                captured_us: 20,
                events: Events::Desktop(vec![InputEvent::Scroll120 { dx: 120, dy: -360 }]),
            }]
        );
        assert!(receiver.receive(old).unwrap().is_empty());
        assert!(receiver.receive(latest).unwrap().is_empty());
    }

    #[test]
    fn desktop_stop_orders_smooth_scroll_before_later_motion_and_scroll() {
        let mut sender = Sender::new(InputMode::Desktop);
        sender
            .push_desktop(InputEvent::ScrollPixels { dx: 1.25, dy: -2.5 }, 10)
            .unwrap();
        sender
            .push_desktop(InputEvent::ScrollStop { cancel: true }, 20)
            .unwrap();
        sender
            .push_desktop(InputEvent::Motion { dx: 3.5, dy: -4.5 }, 30)
            .unwrap();
        sender
            .push_desktop(InputEvent::ScrollPixels { dx: 5.25, dy: 6.5 }, 40)
            .unwrap();
        sender
            .push_desktop(InputEvent::Scroll120 { dx: -120, dy: 240 }, 50)
            .unwrap();

        let mut blocked = update(&sender);
        let stop = blocked.transitions.remove(0);
        let mut receiver = Receiver::new(InputMode::Desktop);
        assert!(receiver.receive(blocked).unwrap().is_empty());

        let complete = Update {
            position: sender.position().unwrap().clone(),
            transitions: vec![stop],
        };
        let deliveries = receiver.receive(complete.clone()).unwrap();
        assert_eq!(deliveries.len(), 2);
        assert_eq!(deliveries[0].captured_us, 20);
        assert_eq!(deliveries[1].captured_us, 50);
        assert_eq!(
            desktop_events(&deliveries),
            vec![
                InputEvent::ScrollPixels { dx: 1.25, dy: -2.5 },
                InputEvent::ScrollStop { cancel: true },
                InputEvent::Motion { dx: 3.5, dy: -4.5 },
                InputEvent::ScrollPixels { dx: 5.25, dy: 6.5 },
                InputEvent::Scroll120 { dx: -120, dy: 240 },
            ]
        );
        assert!(receiver.receive(complete).unwrap().is_empty());
    }

    #[test]
    fn duplicate_desktop_stops_are_distinct_edges_and_retransmits_dedupe() {
        let mut sender = Sender::new(InputMode::Desktop);
        sender
            .push_desktop(InputEvent::ScrollStop { cancel: false }, 1)
            .unwrap();
        sender
            .push_desktop(InputEvent::ScrollStop { cancel: false }, 2)
            .unwrap();
        assert_eq!(sender.pending().len(), 2);
        assert_eq!(sender.pending()[0].id, 0);
        assert_eq!(sender.pending()[1].id, 1);

        let update = update(&sender);
        let mut receiver = Receiver::new(InputMode::Desktop);
        assert_eq!(
            desktop_events(&receiver.receive(update.clone()).unwrap()),
            vec![
                InputEvent::ScrollStop { cancel: false },
                InputEvent::ScrollStop { cancel: false },
            ]
        );
        assert!(receiver.receive(update).unwrap().is_empty());
    }

    #[test]
    fn desktop_discrete_wheel_checks_totals_and_splits_receiver_deltas() {
        let mut sender = Sender::new(InputMode::Desktop);
        sender.position = Some(Position {
            serial: 7,
            captured_us: 1,
            totals: desktop_totals((0.0, 0.0, 0.0, 0.0, i64::MAX, i64::MIN)),
            barrier: 0,
        });
        sender.serial = 7;
        assert!(sender
            .push_desktop(InputEvent::Scroll120 { dx: 1, dy: 0 }, 2)
            .is_err());
        assert!(sender
            .push_desktop(InputEvent::Scroll120 { dx: 0, dy: -1 }, 2)
            .is_err());
        assert_eq!(sender.position().unwrap().serial, 7);

        let position = Position {
            serial: 1,
            captured_us: 9,
            totals: desktop_totals((
                0.0,
                0.0,
                0.0,
                0.0,
                i64::from(i32::MAX) + 1,
                i64::from(i32::MIN) - 1,
            )),
            barrier: 0,
        };
        let mut receiver = Receiver::new(InputMode::Desktop);
        assert_eq!(
            desktop_events(
                &receiver
                    .receive(Update {
                        position,
                        transitions: vec![],
                    })
                    .unwrap()
            ),
            vec![
                InputEvent::Scroll120 {
                    dx: i32::MAX,
                    dy: i32::MIN,
                },
                InputEvent::Scroll120 { dx: 1, dy: -1 },
            ]
        );
    }

    #[test]
    fn maximum_supported_raw_keys_and_buttons_are_not_capped_at_sixty_four() {
        let mut sender = Sender::new(InputMode::Raw);
        let mut events = (1..=0x2bf)
            .filter(|code| keyboard_code(*code))
            .map(|code| RawEvent::Key {
                code,
                pressed: true,
            })
            .collect::<Vec<_>>();
        events.extend((1..=8).map(|number| RawEvent::Button {
            number,
            pressed: true,
        }));
        sender.push_raw(&report(0, 1, events)).unwrap();
        assert_eq!(sender.pending().len(), 615);
        let mut receiver = Receiver::new(InputMode::Raw);
        let delivered = raw_events(&receiver.receive(update(&sender)).unwrap());
        assert_eq!(delivered.len(), 615);
        assert!(delivered.contains(&RawEvent::Key {
            code: 0x2bf,
            pressed: true
        }));
        assert!(delivered.contains(&RawEvent::Button {
            number: 8,
            pressed: true
        }));
    }

    #[test]
    fn extreme_raw_cumulative_delta_splits_into_i32_events() {
        let position = Position {
            serial: 1,
            captured_us: 9,
            totals: Totals::Raw {
                x: i64::from(i32::MAX) + 1,
                y: i64::from(i32::MIN) - 1,
                wheel_x: i64::from(i32::MAX) * 2,
                wheel_y: i64::from(i32::MIN) * 2,
            },
            barrier: 0,
        };
        let mut receiver = Receiver::new(InputMode::Raw);
        let events = raw_events(
            &receiver
                .receive(Update {
                    position,
                    transitions: vec![],
                })
                .unwrap(),
        );
        assert_eq!(
            events,
            vec![
                RawEvent::Motion {
                    x: i32::MAX,
                    y: i32::MIN
                },
                RawEvent::Motion { x: 1, y: -1 },
                RawEvent::Wheel {
                    x120: i32::MAX,
                    y120: i32::MIN
                },
                RawEvent::Wheel {
                    x120: i32::MAX,
                    y120: i32::MIN
                }
            ]
        );
    }

    #[test]
    fn receiver_rejects_wrong_modes_edges_and_conflicting_history() {
        let mut receiver = Receiver::new(InputMode::Raw);
        let wrong_mode = Update {
            position: Position {
                serial: 1,
                captured_us: 1,
                totals: desktop_totals((0.0, 0.0, 0.0, 0.0, 0, 0)),
                barrier: 0,
            },
            transitions: vec![],
        };
        assert!(receiver.receive(wrong_mode).is_err());

        let position = Position {
            serial: 1,
            captured_us: 1,
            totals: raw_totals((0, 0, 0, 0)),
            barrier: 2,
        };
        let future = Transition {
            id: 1,
            position: Position {
                barrier: 1,
                ..position.clone()
            },
            event: InputEvent::Key {
                code: 30,
                pressed: true,
            },
        };
        receiver
            .receive(Update {
                position: position.clone(),
                transitions: vec![future.clone()],
            })
            .unwrap();
        let mut conflicting = future;
        conflicting.event = InputEvent::Key {
            code: 31,
            pressed: true,
        };
        assert!(receiver
            .receive(Update {
                position,
                transitions: vec![conflicting],
            })
            .is_err());

        let mut sender = Sender::new(InputMode::Raw);
        sender
            .push_raw(&report(
                0,
                4,
                vec![RawEvent::Key {
                    code: 30,
                    pressed: true,
                }],
            ))
            .unwrap();
        let valid = update(&sender);
        let mut receiver = Receiver::new(InputMode::Raw);
        receiver.receive(valid.clone()).unwrap();
        let mut altered = valid;
        altered.transitions[0].event = InputEvent::Key {
            code: 31,
            pressed: true,
        };
        assert!(receiver.receive(altered).is_err());
    }

    #[test]
    fn reordered_old_positions_do_not_rollback_motion() {
        let mut sender = Sender::new(InputMode::Raw);
        sender
            .push_raw(&report(0, 1, vec![RawEvent::Motion { x: 5, y: 0 }]))
            .unwrap();
        let old = update(&sender);
        sender
            .push_raw(&report(1, 2, vec![RawEvent::Motion { x: 7, y: 0 }]))
            .unwrap();
        let new = update(&sender);
        let mut receiver = Receiver::new(InputMode::Raw);
        assert_eq!(
            raw_events(&receiver.receive(new).unwrap()),
            vec![RawEvent::Motion { x: 12, y: 0 }]
        );
        assert!(receiver.receive(old).unwrap().is_empty());
    }
}
