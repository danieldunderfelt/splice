use super::*;
use serde::Serialize;

#[derive(Serialize)]
pub struct DeviceReport {
    product: Option<String>,
    transport: Option<String>,
    vendor_id: Option<i64>,
    product_id: Option<i64>,
    report_interval_us: Option<i64>,
    descriptor_hex: Option<String>,
    relative_mouse: bool,
    keyboard: bool,
    descriptor_error: Option<String>,
    connected: bool,
    callbacks: BTreeMap<u32, CallbackReport>,
}

#[derive(Default, Serialize)]
struct CallbackReport {
    count: u64,
    byte_lengths: std::collections::BTreeSet<isize>,
    invalid_reports: u64,
    last_error: Option<String>,
}

struct ProbeDevice {
    decoder: Option<Decoder>,
    report: DeviceReport,
}

#[derive(Default)]
struct Probe {
    devices: BTreeMap<usize, ProbeDevice>,
    overflow: bool,
}

pub fn inspect(duration: Duration) -> anyhow::Result<Vec<DeviceReport>> {
    anyhow::ensure!(
        (1..=30).contains(&duration.as_secs()),
        "probe duration must be 1 to 30 seconds"
    );
    anyhow::ensure!(
        unsafe { IOHIDCheckAccess(1) } == 0,
        "Input Monitoring permission is required for the HID probe"
    );
    let mut probe = Probe::default();
    let manager = unsafe {
        HidManager::open(
            &mut probe as *mut Probe as Ref,
            matched,
            disconnected,
            input,
        )?
    };
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        CFRunLoop::run_in_mode(
            unsafe { kCFRunLoopDefaultMode },
            Duration::from_millis(20),
            false,
        );
    }
    drop(manager);
    anyhow::ensure!(
        !probe.overflow,
        "HID probe exceeded its device or report-ID limit"
    );
    Ok(probe.devices.into_values().map(|d| d.report).collect())
}

unsafe fn string(device: Ref, key: &str) -> Option<String> {
    property(device, key)
        .and_then(|v| v.downcast::<CFString>())
        .map(|s| s.to_string())
}

unsafe fn number(device: Ref, key: &str) -> Option<i64> {
    property(device, key)
        .and_then(|v| v.downcast::<CFNumber>())
        .and_then(|n| n.to_i64())
}

unsafe extern "C" fn matched(context: Ref, result: i32, _: Ref, device: Ref) {
    let probe = &mut *(context as *mut Probe);
    if probe.devices.len() >= MAX_DEVICES {
        probe.overflow = true;
        return;
    }
    let parsed = read_decoder(device, result);
    let report = DeviceReport {
        product: string(device, "Product"),
        transport: string(device, "Transport"),
        vendor_id: number(device, "VendorID"),
        product_id: number(device, "ProductID"),
        report_interval_us: number(device, "ReportInterval"),
        descriptor_hex: property(device, "ReportDescriptor")
            .and_then(|v| v.downcast::<CFData>())
            .filter(|v| v.len() <= 4096)
            .map(|data| data.bytes().iter().map(|b| format!("{b:02x}")).collect()),
        relative_mouse: parsed.as_ref().is_ok_and(|d| d.mouse),
        keyboard: parsed.as_ref().is_ok_and(|d| d.keyboard),
        descriptor_error: parsed.as_ref().err().map(|e| format!("{e:#}")),
        connected: true,
        callbacks: BTreeMap::new(),
    };
    probe.devices.insert(
        device as usize,
        ProbeDevice {
            decoder: parsed.ok(),
            report,
        },
    );
}

unsafe extern "C" fn disconnected(context: Ref, _: i32, _: Ref, device: Ref) {
    let probe = &mut *(context as *mut Probe);
    if let Some(device) = probe.devices.get_mut(&(device as usize)) {
        device.report.connected = false;
    }
}

unsafe extern "C" fn input(
    context: Ref,
    result: i32,
    sender: Ref,
    kind: u32,
    id: u32,
    bytes: *mut u8,
    length: isize,
) {
    let probe = &mut *(context as *mut Probe);
    if id > 255 {
        probe.overflow = true;
        return;
    }
    let Some(device) = probe.devices.get_mut(&(sender as usize)) else {
        return;
    };
    let stats = device.report.callbacks.entry(id).or_default();
    stats.count += 1;
    if stats.byte_lengths.len() < 32 {
        stats.byte_lengths.insert(length);
    }
    let mut decode = || -> anyhow::Result<()> {
        anyhow::ensure!(
            result == 0 && kind == 0 && !bytes.is_null() && (1..=4096).contains(&length),
            "invalid HID callback status, kind, or length"
        );
        let bytes = std::slice::from_raw_parts(bytes, length as usize);
        let payload = if id == 0 {
            bytes
        } else {
            anyhow::ensure!(
                bytes[0] == id as u8,
                "HID report ID does not match its data"
            );
            &bytes[1..]
        };
        if let Some(decoder) = &mut device.decoder {
            decoder.decode(id as u8, payload)?;
        }
        Ok(())
    };
    if let Err(error) = (decode)() {
        stats.invalid_reports += 1;
        stats.last_error = Some(format!("{error:#}"));
    }
}
