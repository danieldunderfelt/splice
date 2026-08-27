//! NSPasteboard integration: a `changeCount` poller for local changes, and promised
//! (lazily provided) items for remote offers.

use super::MacShared;
use crate::{ClipFetch, Clipboard, ClipboardOffer, PlatformError, PlatformEvent, Result};
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, AnyThread, DefinedClass};
use objc2_app_kit::{
    NSBitmapImageFileType, NSBitmapImageRep, NSPasteboard, NSPasteboardItem,
    NSPasteboardItemDataProvider, NSPasteboardType, NSPasteboardTypeString, NSPasteboardWriting,
};
use objc2_foundation::{NSArray, NSData, NSDictionary, NSString};
use splice_proto::CLIP_INLINE_TEXT_MAX;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

const POLL_INTERVAL: Duration = Duration::from_millis(500);
const FETCH_TIMEOUT: Duration = Duration::from_secs(2);

pub const MIME_TEXT: &str = "text/plain;charset=utf-8";
pub const MIME_HTML: &str = "text/html";
pub const MIME_PNG: &str = "image/png";
pub const MIME_RTF: &str = "text/rtf";

const UTI_TEXT: &str = "public.utf8-plain-text";
const UTI_HTML: &str = "public.html";
const UTI_PNG: &str = "public.png";
const UTI_TIFF: &str = "public.tiff";
const UTI_RTF: &str = "public.rtf";

/// UTI → wire MIME. Both image UTIs normalize to PNG (macOS pasteboards are TIFF-first).
pub fn uti_to_mime(uti: &str) -> Option<&'static str> {
    match uti {
        UTI_TEXT => Some(MIME_TEXT),
        UTI_HTML => Some(MIME_HTML),
        UTI_PNG | UTI_TIFF => Some(MIME_PNG),
        UTI_RTF => Some(MIME_RTF),
        _ => None,
    }
}

/// Wire MIME → the UTI Splice writes when serving a promised item.
pub fn mime_to_uti(mime: &str) -> Option<&'static str> {
    match mime {
        MIME_TEXT => Some(UTI_TEXT),
        MIME_HTML => Some(UTI_HTML),
        MIME_PNG => Some(UTI_PNG),
        MIME_RTF => Some(UTI_RTF),
        _ => None,
    }
}

/// Normalized MIME list in preference order (richest first), deduped.
pub fn normalize(utis: &[String]) -> Vec<String> {
    let present: Vec<&'static str> = utis.iter().filter_map(|u| uti_to_mime(u)).collect();
    [MIME_PNG, MIME_HTML, MIME_RTF, MIME_TEXT]
        .into_iter()
        .filter(|m| present.contains(m))
        .map(str::to_string)
        .collect()
}

pub struct PasteboardClip {
    shared: Arc<MacShared>,
    /// changeCount produced by our own writes, skipped by the poller (loop guard).
    own_change: Arc<AtomicI64>,
    runtime: tokio::runtime::Handle,
}

impl PasteboardClip {
    pub fn new(shared: Arc<MacShared>) -> Self {
        let own_change = Arc::new(AtomicI64::new(-1));
        let this = Self {
            shared: shared.clone(),
            own_change: own_change.clone(),
            runtime: tokio::runtime::Handle::current(),
        };
        std::thread::Builder::new()
            .name("splice-pasteboard".into())
            .spawn(move || poll_loop(shared, own_change))
            .expect("spawning the pasteboard poller");
        this
    }
}

/// NSPasteboard has no change notification; polling changeCount is cheap and is what every
/// macOS clipboard manager does.
fn poll_loop(shared: Arc<MacShared>, own_change: Arc<AtomicI64>) {
    let mut last = pasteboard_change_count();
    loop {
        std::thread::sleep(POLL_INTERVAL);
        let count = pasteboard_change_count();
        if count == last || count == own_change.load(Ordering::SeqCst) {
            last = count;
            continue;
        }
        last = count;
        objc2::rc::autoreleasepool(|_| {
            let pb = NSPasteboard::generalPasteboard();
            let Some(types) = pb.types() else { return };
            let utis: Vec<String> = types.iter().map(|t| t.to_string()).collect();
            let mimes = normalize(&utis);
            if mimes.is_empty() {
                return;
            }
            // Content reads are gated to what we actually offer: small text only.
            let inline_text = pb
                .stringForType(unsafe { NSPasteboardTypeString })
                .map(|s| s.to_string())
                .filter(|s| s.len() <= CLIP_INLINE_TEXT_MAX);
            shared.emit(PlatformEvent::ClipboardChanged { mimes, inline_text });
        });
    }
}

fn pasteboard_change_count() -> i64 {
    objc2::rc::autoreleasepool(|_| NSPasteboard::generalPasteboard().changeCount()) as i64
}

fn ns(s: &str) -> Retained<NSString> {
    NSString::from_str(s)
}

fn read_uti(pb: &NSPasteboard, uti: &str) -> Option<Vec<u8>> {
    pb.dataForType(&ns(uti)).map(|d| d.to_vec())
}

fn tiff_to_png(tiff: &[u8]) -> Option<Vec<u8>> {
    let data = NSData::with_bytes(tiff);
    let rep = NSBitmapImageRep::imageRepWithData(&data)?;
    let props = NSDictionary::new();
    let png = unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &props) }?;
    Some(png.to_vec())
}

#[async_trait::async_trait]
impl Clipboard for PasteboardClip {
    async fn read_local(&self, mime: &str) -> Result<Vec<u8>> {
        let mime = mime.to_string();
        objc2::rc::autoreleasepool(|_| {
            let pb = NSPasteboard::generalPasteboard();
            let data = match mime.as_str() {
                MIME_PNG => read_uti(&pb, UTI_PNG)
                    .or_else(|| read_uti(&pb, UTI_TIFF).and_then(|t| tiff_to_png(&t))),
                other => mime_to_uti(other).and_then(|uti| read_uti(&pb, uti)),
            };
            data.ok_or_else(|| {
                PlatformError::Unavailable(format!("clipboard has no representation for {mime}"))
            })
        })
    }

    async fn set_remote_offer(&self, offer: ClipboardOffer, fetch: Arc<dyn ClipFetch>) -> Result<()> {
        let utis: Vec<&'static str> = offer.mimes.iter().filter_map(|m| mime_to_uti(m)).collect();
        if utis.is_empty() {
            return Ok(());
        }
        let ctx = Arc::new(ProviderCtx {
            fetch,
            inline_text: offer.inline_text.clone(),
            runtime: self.runtime.clone(),
        });
        let count = objc2::rc::autoreleasepool(|_| {
            let pb = NSPasteboard::generalPasteboard();
            pb.clearContents();
            let item = NSPasteboardItem::new();
            let types: Vec<Retained<NSPasteboardType>> = utis.iter().map(|u| ns(u)).collect();
            let provider = Provider::new(ctx);
            let provider = ProtocolObject::from_ref(&*provider);
            item.setDataProvider_forTypes(provider, &NSArray::from_retained_slice(&types));
            let writable: &ProtocolObject<dyn NSPasteboardWriting> =
                ProtocolObject::from_ref(&*item);
            pb.writeObjects(&NSArray::from_slice(&[writable]));
            pb.changeCount() as i64
        });
        self.own_change.store(count, Ordering::SeqCst);
        self.shared.set_health(|h| h.clipboard = None);
        Ok(())
    }
}

struct ProviderCtx {
    fetch: Arc<dyn ClipFetch>,
    inline_text: Option<String>,
    runtime: tokio::runtime::Handle,
}

impl ProviderCtx {
    /// The provider is called synchronously on whatever thread the pasting app's read
    /// lands on — usually not a tokio worker, but we must not assume it. Blocking a
    /// runtime worker with `block_on` would deadlock the executor, so in that case the
    /// fetch is handed to the runtime and awaited over a plain channel instead.
    fn fetch_blocking(&self, mime: &str) -> Option<Vec<u8>> {
        if tokio::runtime::Handle::try_current().is_ok() {
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            let fetch = self.fetch.clone();
            let mime = mime.to_string();
            self.runtime.spawn(async move {
                let _ = tx.send(fetch.fetch(&mime).await);
            });
            rx.recv_timeout(FETCH_TIMEOUT).ok().flatten()
        } else {
            let fetch = self.fetch.clone();
            let mime = mime.to_string();
            self.runtime.block_on(async move {
                tokio::time::timeout(FETCH_TIMEOUT, fetch.fetch(&mime))
                    .await
                    .ok()
                    .flatten()
            })
        }
    }
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and Provider has no Drop impl.
    #[unsafe(super(NSObject))]
    #[name = "SpliceClipboardProvider"]
    #[ivars = Arc<ProviderCtx>]
    struct Provider;

    unsafe impl NSObjectProtocol for Provider {}

    unsafe impl NSPasteboardItemDataProvider for Provider {
        #[unsafe(method(pasteboard:item:provideDataForType:))]
        fn provide_data(
            &self,
            _pasteboard: Option<&NSPasteboard>,
            item: &NSPasteboardItem,
            uti: &NSPasteboardType,
        ) {
            let uti = uti.to_string();
            let Some(mime) = uti_to_mime(&uti) else { return };
            let ctx = self.ivars();
            // Small text rides along in the offer, so paste works even if the origin left.
            if mime == MIME_TEXT {
                if let Some(text) = &ctx.inline_text {
                    item.setData_forType(&NSData::with_bytes(text.as_bytes()), &ns(&uti));
                    return;
                }
            }
            let bytes = ctx.fetch_blocking(mime).unwrap_or_default();
            item.setData_forType(&NSData::with_bytes(&bytes), &ns(&uti));
        }
    }
);

impl Provider {
    fn new(ctx: Arc<ProviderCtx>) -> Retained<Self> {
        let this: Allocated<Self> = Self::alloc();
        let this = this.set_ivars(ctx);
        unsafe { msg_send![super(this), init] }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uti_mapping_is_symmetric_where_it_can_be() {
        for mime in [MIME_TEXT, MIME_HTML, MIME_PNG, MIME_RTF] {
            let uti = mime_to_uti(mime).expect("mime maps to a uti");
            assert_eq!(uti_to_mime(uti), Some(mime));
        }
        // TIFF is read-only on our side: it normalizes to PNG on the wire.
        assert_eq!(uti_to_mime(UTI_TIFF), Some(MIME_PNG));
    }

    #[test]
    fn normalize_orders_richest_first_and_dedupes_images() {
        let utis = vec![UTI_TEXT.to_string(), UTI_TIFF.to_string(), UTI_PNG.to_string()];
        assert_eq!(normalize(&utis), vec![MIME_PNG, MIME_TEXT]);
    }

    #[test]
    fn normalize_drops_unknown_utis() {
        assert!(normalize(&["public.file-url".to_string()]).is_empty());
    }
}
