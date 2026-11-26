//! AppKit file-list drops, routed by the same hit testing as pointer input.
//!
//! A drag offers local files (`public.file-url`: Finder), which drop as
//! their URLs. A chat app (WeChat, WeCom, Feishu) may offer a picture as
//! its data alone, or beside the URL of a file in its own container that
//! other apps may not read; a browser or Mail may only promise a file
//! (`NSFilePromiseReceiver`). Those are written to a folder of the drop's
//! own in the temporary folder, and drop as the URLs of the files written
//! (a promise's once AppKit has written them). Such apps often offer only
//! the generic operation, not a copy: either is taken.
use std::{
    cell::{Cell, RefCell},
    path::PathBuf,
    ptr::NonNull,
    sync::{Arc, Mutex},
};

use block2::RcBlock;
use nsh_core::{EventKind, EventPayload};
use objc2::{ClassType, rc::Retained, runtime::ProtocolObject};
use objc2_app_kit::{NSDragOperation, NSDraggingInfo, NSFilePromiseReceiver, NSView};
use objc2_foundation::{NSArray, NSDictionary, NSError, NSOperationQueue, NSString, NSURL};

use crate::{input, surface::Surface, view::NshView};

#[derive(Default)]
pub(crate) struct State {
    target: RefCell<Option<NshView>>,
    offered: Cell<bool>,
    /// The queue promised files are written on, kept while they are.
    promise_queue: std::cell::OnceCell<Retained<NSOperationQueue>>,
    destinations: RefCell<Destinations>,
}

#[derive(Default)]
struct Destinations {
    enabled: Vec<bool>,
    count: usize,
}
impl Destinations {
    fn set(&mut self, id: nsh_core::NodeId, enabled: bool) -> Option<bool> {
        let index = id as usize;
        if self.enabled.get(index).copied().unwrap_or(false) == enabled {
            return None;
        }
        let before = self.count != 0;
        if enabled {
            self.enabled
                .resize(self.enabled.len().max(index + 1), false);
            self.count += 1;
        } else {
            self.count -= 1;
        }
        self.enabled[index] = enabled;
        let after = self.count != 0;
        (before != after).then_some(after)
    }
}

pub(crate) fn events_changed(surface: &Surface, view: &NshView) {
    let enabled = view.state().events.contains(EventKind::FileDrop);
    destination_changed(surface, view.id(), enabled);
}

pub(crate) fn deleted(surface: &Surface, id: nsh_core::NodeId) {
    destination_changed(surface, id, false);
}

fn destination_changed(surface: &Surface, id: nsh_core::NodeId, enabled: bool) {
    let registration = surface.file_drop.destinations.borrow_mut().set(id, enabled);
    match registration {
        Some(true) => {
            register(surface.root.ns());
            for root in crate::window::roots(surface) {
                register(root.ns());
            }
        }
        Some(false) => {
            surface.root.ns().unregisterDraggedTypes();
            for root in crate::window::roots(surface) {
                root.ns().unregisterDraggedTypes();
            }
        }
        None => {}
    }
    if !enabled {
        invalidate(surface, id);
    }
}

pub(crate) fn reset(surface: &Surface) {
    clear(surface);
    if surface.file_drop.destinations.borrow().count > 0 {
        surface.root.ns().unregisterDraggedTypes();
    }
    *surface.file_drop.destinations.borrow_mut() = Destinations::default();
}

/// Whether a view of the surface takes file drops: a window's root view
/// made later registers then ([`crate::window`]).
pub(crate) fn accepting(surface: &Surface) -> bool {
    surface.file_drop.destinations.borrow().count > 0
}

pub(crate) fn register(view: &NSView) {
    let mut kinds = vec![NSString::from_str("public.file-url")];
    kinds.extend(IMAGE_DATA.iter().map(|(kind, _)| NSString::from_str(kind)));
    kinds.extend(NSFilePromiseReceiver::readableDraggedTypes().to_vec());
    let kinds = kinds.iter().map(|kind| &**kind).collect::<Vec<_>>();
    view.registerForDraggedTypes(&NSArray::from_slice(&kinds));
}

/// The operation a drop of files makes for `sender`'s drag: a copy, or the
/// generic operation of a source that offers no copy (a chat app's
/// picture); never a move or a link.
fn operation(sender: &ProtocolObject<dyn NSDraggingInfo>) -> NSDragOperation {
    let mask = sender.draggingSourceOperationMask();
    if mask.contains(NSDragOperation::Copy) {
        NSDragOperation::Copy
    } else if mask.contains(NSDragOperation::Generic) {
        NSDragOperation::Generic
    } else {
        NSDragOperation::None
    }
}

fn target(surface: &Surface, sender: &ProtocolObject<dyn NSDraggingInfo>) -> Option<NshView> {
    if operation(sender).is_empty() {
        return None;
    }
    let point = input::event_root(surface)
        .ns()
        .convertPoint_fromView(sender.draggingLocation(), None);
    input::file_drop_target(surface, point.x as f32, point.y as f32)
}

fn set_target(surface: &Surface, target: Option<NshView>) {
    let mut current = surface.file_drop.target.borrow_mut();
    if current.as_ref().map(NshView::id) == target.as_ref().map(NshView::id) {
        return;
    }
    if let Some(previous) = current.take()
        && previous.state().events.contains(EventKind::FileDragLeave)
    {
        surface.event(previous.id(), EventKind::FileDragLeave, EventPayload::None);
    }
    if let Some(next) = &target
        && next.state().events.contains(EventKind::FileDragEnter)
    {
        surface.event(next.id(), EventKind::FileDragEnter, EventPayload::None);
    }
    *current = target;
}

pub(crate) fn clear(surface: &Surface) {
    surface.file_drop.offered.set(false);
    set_target(surface, None);
}

pub(crate) fn invalidate(surface: &Surface, id: nsh_core::NodeId) {
    let matches = surface
        .file_drop
        .target
        .borrow()
        .as_ref()
        .is_some_and(|view| view.id() == id);
    if matches {
        set_target(surface, None);
    }
}

pub(crate) fn begin(
    surface: &Surface,
    sender: &ProtocolObject<dyn NSDraggingInfo>,
) -> NSDragOperation {
    surface
        .file_drop
        .offered
        .set(!offers(sender).is_empty() || !promises(sender).is_empty());
    update(surface, sender)
}

pub(crate) fn update(
    surface: &Surface,
    sender: &ProtocolObject<dyn NSDraggingInfo>,
) -> NSDragOperation {
    let next = if surface.file_drop.offered.get() {
        target(surface, sender)
    } else {
        None
    };
    let operation = if next.is_some() {
        operation(sender)
    } else {
        NSDragOperation::None
    };
    set_target(surface, next);
    operation
}

pub(crate) fn perform(surface: &Surface, sender: &ProtocolObject<dyn NSDraggingInfo>) -> bool {
    let Some(destination) = target(surface, sender) else {
        clear(surface);
        return false;
    };
    let offers = offers(sender);
    if offers.is_empty() {
        let promised = drop_promises(surface, &destination, promises(sender));
        clear(surface);
        return promised;
    }
    let uris = uris_of(sender, offers);
    if uris.is_empty() {
        clear(surface);
        return false;
    }
    set_target(surface, Some(destination.clone()));
    surface.event(
        destination.id(),
        EventKind::FileDrop,
        EventPayload::FileDrop(uris),
    );
    clear(surface);
    true
}

/// The types a picture dragged as its data may come in, most wanted first,
/// with the extension of the file it is written to (TIFF, AppKit's own,
/// is written as PNG).
const IMAGE_DATA: [(&str, &str); 4] = [
    ("public.png", "png"),
    ("public.jpeg", "jpg"),
    ("com.compuserve.gif", "gif"),
    ("public.tiff", "png"),
];

/// What one item of a drag drops as.
enum Offer {
    /// A local file or directory this app can read, by its URL.
    Uri(String),
    /// A picture as its data (pasteboard item `item`, of type `kind`),
    /// named after the file the item names, if any.
    Data {
        item: usize,
        kind: &'static str,
        ext: &'static str,
        name: Option<String>,
    },
}

/// What each item of the drag drops as: its file, when this app can read
/// it; else the picture it carries, if any; else its file all the same
/// (the app finds it cannot read it). Items with neither are left out.
fn offers(sender: &ProtocolObject<dyn NSDraggingInfo>) -> Vec<Offer> {
    let file = NSString::from_str("public.file-url");
    let Some(items) = sender.draggingPasteboard().pasteboardItems() else {
        return Vec::new();
    };
    items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            let uri = item
                .stringForType(&file)
                .and_then(|uri| local_file_uri(&uri));
            let path = uri.as_deref().and_then(path_of);
            if let Some(uri) = &uri
                && path
                    .as_ref()
                    .is_some_and(|path| path.is_dir() || std::fs::File::open(path).is_ok())
            {
                return Some(Offer::Uri(uri.clone()));
            }
            let kinds = item.types();
            let data = IMAGE_DATA
                .iter()
                .find(|(kind, _)| kinds.iter().any(|offered| offered.to_string() == *kind));
            match data {
                Some(&(kind, ext)) => Some(Offer::Data {
                    item: index,
                    kind,
                    ext,
                    name: path.as_ref().and_then(|path| {
                        path.file_stem()
                            .map(|stem| stem.to_string_lossy().into_owned())
                    }),
                }),
                None => uri.map(Offer::Uri),
            }
        })
        .collect()
}

/// The path of a `file:` URL.
fn path_of(uri: &str) -> Option<PathBuf> {
    NSURL::URLWithString(&NSString::from_str(uri))
        .and_then(|url| url.path())
        .map(|path| PathBuf::from(path.to_string()))
}

/// The URLs `offers` drop as: a file's own, a picture's once written to the
/// drop's folder.
fn uris_of(sender: &ProtocolObject<dyn NSDraggingInfo>, offers: Vec<Offer>) -> Vec<String> {
    let items = sender.draggingPasteboard().pasteboardItems();
    let mut folder = None;
    let mut uris = Vec::new();
    for (n, offer) in offers.into_iter().enumerate() {
        match offer {
            Offer::Uri(uri) => uris.push(uri),
            Offer::Data {
                item,
                kind,
                ext,
                name,
            } => {
                let data = items
                    .as_ref()
                    .and_then(|items| items.iter().nth(item))
                    .and_then(|item| item.dataForType(&NSString::from_str(kind)))
                    .map(|data| data.to_vec());
                let data = match (kind, data) {
                    ("public.tiff", Some(tiff)) => png_of(&tiff),
                    (_, data) => data,
                };
                let Some(data) = data else { continue };
                if folder.is_none() {
                    folder = drop_folder();
                }
                let Some(folder) = &folder else { continue };
                let stem = name.unwrap_or_else(|| format!("image-{}", n + 1));
                let file = folder.join(format!("{stem}.{ext}"));
                match std::fs::write(&file, &data) {
                    Ok(()) => uris.extend(file_uri(&file)),
                    Err(e) => log::warn!("cannot write {}: {e}", file.display()),
                }
            }
        }
    }
    uris
}

/// A TIFF picture (AppKit's own pasteboard picture) as a PNG.
fn png_of(tiff: &[u8]) -> Option<Vec<u8>> {
    use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep, NSBitmapImageRepPropertyKey};
    use objc2_foundation::NSData;
    let rep = NSBitmapImageRep::imageRepWithData(&NSData::with_bytes(tiff))?;
    let properties = NSDictionary::<NSBitmapImageRepPropertyKey, objc2::runtime::AnyObject>::new();
    // SAFETY: an empty property dictionary is valid for PNG.
    let png =
        unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &properties) };
    png.map(|data| data.to_vec())
}

/// The `file:` URL of `path`.
fn file_uri(path: &std::path::Path) -> Option<String> {
    let url = NSURL::fileURLWithPath(&NSString::from_str(path.to_str()?));
    url.absoluteString().map(|uri| uri.to_string())
}

/// A new folder for the files one drop writes, in the temporary folder.
/// The app reads them when it gets the drop, after this returns: the
/// folders of drops an hour old go then, not at once.
fn drop_folder() -> Option<PathBuf> {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let root = std::env::temp_dir().join("nsh-drops");
    if let Ok(old) = std::fs::read_dir(&root) {
        for entry in old.flatten() {
            let stale = entry
                .metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|at| at.elapsed().is_ok_and(|age| age.as_secs() > 3600));
            if stale {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
    let folder = root.join(format!(
        "{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&folder)
        .inspect_err(|e| log::warn!("cannot make {}: {e}", folder.display()))
        .ok()?;
    Some(folder)
}

/// The file promises on the drag's pasteboard.
fn promises(sender: &ProtocolObject<dyn NSDraggingInfo>) -> Vec<Retained<NSFilePromiseReceiver>> {
    let board = sender.draggingPasteboard();
    let classes = NSArray::from_slice(&[NSFilePromiseReceiver::class()]);
    // SAFETY: an array of a class that reads from pasteboards, no options.
    let Some(objects) = (unsafe { board.readObjectsForClasses_options(&classes, None) }) else {
        return Vec::new();
    };
    objects
        .iter()
        .filter_map(|object| object.downcast::<NSFilePromiseReceiver>().ok())
        .collect()
}

/// Drops the files `receivers` promise on `destination`, once AppKit has
/// written them all to the drop's folder (in the drag's order, those that
/// failed left out): whether there were any.
fn drop_promises(
    surface: &Surface,
    destination: &NshView,
    receivers: Vec<Retained<NSFilePromiseReceiver>>,
) -> bool {
    let count = receivers
        .iter()
        .map(|receiver| receiver.fileTypes().count())
        .sum::<usize>();
    let mut control = None;
    surface.control(|c| control = Some(c.clone()));
    let (Some(control), true) = (control, count > 0) else {
        return false;
    };
    let Some(folder) = drop_folder() else {
        return false;
    };
    let Some(folder_url) = folder
        .to_str()
        .map(|path| NSURL::fileURLWithPath_isDirectory(&NSString::from_str(path), true))
    else {
        return false;
    };
    let id = destination.id();
    let written = Arc::new(Mutex::new(Vec::<(usize, Option<PathBuf>)>::new()));
    let queue = surface
        .file_drop
        .promise_queue
        .get_or_init(NSOperationQueue::new);
    for (order, receiver) in receivers.iter().enumerate() {
        let (written, control) = (written.clone(), control.clone());
        let reader = RcBlock::new(move |url: NonNull<NSURL>, error: *mut NSError| {
            // SAFETY: AppKit hands the reader a valid URL.
            let url = unsafe { url.as_ref() };
            let path = if error.is_null() {
                url.path().map(|path| PathBuf::from(path.to_string()))
            } else {
                // SAFETY: a non-null error is a valid `NSError`.
                let error = unsafe { &*error };
                log::warn!(
                    "a dropped file was not written: {}",
                    error.localizedDescription()
                );
                None
            };
            let mut written = written.lock().unwrap_or_else(|e| e.into_inner());
            written.push((order, path));
            if written.len() != count {
                return;
            }
            written.sort_by_key(|(order, _)| *order);
            let uris = written
                .drain(..)
                .filter_map(|(_, path)| path.as_deref().and_then(file_uri))
                .collect::<Vec<_>>();
            if !uris.is_empty() {
                control.dispatch_event(id, EventKind::FileDrop, EventPayload::FileDrop(uris));
            }
        });
        // SAFETY: an empty options dictionary; the reader only moves `Send`
        // data between AppKit's queue and the engine.
        unsafe {
            receiver.receivePromisedFilesAtDestination_options_operationQueue_reader(
                &folder_url,
                &NSDictionary::new(),
                queue,
                &reader,
            );
        }
    }
    true
}

/// The `file:` URL with the path of the local file or directory `uri`
/// names, if it names one. Finder puts file reference URLs
/// (`file:///.file/id=…`) on drag and copy pasteboards: they become the
/// path's URL, which apps can turn into a path.
pub(crate) fn local_file_uri(uri: &NSString) -> Option<String> {
    let url = NSURL::URLWithString(uri)?.filePathURL()?;
    let local = url.isFileURL()
        && url
            .path()
            .is_some_and(|path| path.to_string().starts_with('/'))
        && url.host().is_none_or(|host| {
            let host = host.to_string();
            host.is_empty() || host.eq_ignore_ascii_case("localhost")
        });
    local
        .then(|| url.absoluteString())
        .flatten()
        .map(|uri| uri.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registration_lasts_until_the_final_destination_is_removed() {
        let mut destinations = Destinations::default();
        assert_eq!(destinations.set(1, false), None);
        assert_eq!(destinations.set(2, true), Some(true));
        assert_eq!(destinations.set(5, true), None);
        assert_eq!(destinations.set(2, false), None);
        assert_eq!(destinations.set(2, false), None);
        assert_eq!(destinations.set(5, false), Some(false));
        assert_eq!(destinations.set(5, true), Some(true));
    }

    #[test]
    fn file_urls_are_local_and_absolute() {
        for uri in [
            "file:///tmp/a%20b",
            "file:///tmp/%E4%B8%AD/",
            "file://localhost/tmp/a",
        ] {
            assert!(local_file_uri(&NSString::from_str(uri)).is_some(), "{uri}");
        }
        for uri in [
            "https://example.com/file",
            "file://server/tmp/a",
            "file:relative",
            "plain text",
        ] {
            assert_eq!(local_file_uri(&NSString::from_str(uri)), None, "{uri}");
        }
    }

    #[test]
    fn file_reference_urls_become_the_paths_urls() {
        let dir = std::env::temp_dir().join("nsh file drop 中");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a b.txt");
        std::fs::write(&file, "x").unwrap();
        let path = NSString::from_str(&file.to_string_lossy());
        let path_url = NSURL::fileURLWithPath(&path);
        let reference = path_url
            .fileReferenceURL()
            .unwrap()
            .absoluteString()
            .unwrap();
        assert!(
            reference.to_string().starts_with("file:///.file/id="),
            "{reference}"
        );
        // The reference resolves to the file's real path (`/var` is `/private/var`).
        let real = std::fs::canonicalize(&file).unwrap();
        let expected = NSURL::fileURLWithPath(&NSString::from_str(&real.to_string_lossy()))
            .absoluteString()
            .unwrap()
            .to_string();
        assert_eq!(
            local_file_uri(&reference).as_deref(),
            Some(expected.as_str())
        );
        // A reference to nothing (the file is gone) is no file.
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(local_file_uri(&reference), None);
    }
}
