//! Where a multiline input says an input method's marked text is, when the
//! input lies outside what can be seen (as the rich editor's sink reaches
//! past its editor's edge): AppKit's own answer is the zero rect, the
//! screen's corner, where input methods then open their candidates. AppKit
//! views live on the main thread, so this test runs without libtest's
//! harness, on the process's own.

#[cfg(target_os = "macos")]
fn main() {
    macos::run();
}

#[cfg(not(target_os = "macos"))]
fn main() {}

#[cfg(target_os = "macos")]
mod macos {
    use std::time::{Duration, Instant};

    use nsh_base::input;
    use nsh_core::{element::Root, prelude::*};
    use nsh_platform_macos::{
        AppOptions, AppView,
        objc2::{
            ClassType, MainThreadMarker, MainThreadOnly, Message, msg_send, rc::Retained,
            runtime::AnyObject,
        },
        objc2_app_kit::{
            NSApplication, NSBackingStoreType, NSTextView, NSView, NSWindow, NSWindowStyleMask,
        },
        objc2_foundation::{NSDate, NSPoint, NSRange, NSRect, NSRunLoop, NSSize, NSString},
    };

    /// An input wholly left of the window's content, as the sink is left of
    /// its editor when the caret is at the editor's start.
    #[component]
    fn Outside() -> impl IntoElement {
        view().w_full().h_full().child(
            input()
                .absolute()
                .left(px(-650.0))
                .top(px(100.0))
                .w(px(600.0))
                .h(px(16.0))
                .multiline(true)
                .text_right(),
        )
    }

    fn spin(seconds: f64) {
        NSRunLoop::currentRunLoop().runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(seconds));
    }

    fn text_view(view: &NSView) -> Option<Retained<NSTextView>> {
        if let Ok(t) = view.retain().downcast::<NSTextView>() {
            return Some(t);
        }
        view.subviews().iter().find_map(|c| text_view(&c))
    }

    fn first_rect(view: &NSTextView, range: NSRange) -> NSRect {
        // SAFETY: NSTextInputClient's method; no actual range is asked for.
        unsafe {
            msg_send![view, firstRectForCharacterRange: range, actualRange: std::ptr::null_mut::<NSRange>()]
        }
    }

    pub(super) fn run() {
        let mtm = MainThreadMarker::new().expect("the main thread");
        let _app = NSApplication::sharedApplication(mtm);
        let frame = NSRect::new(NSPoint::new(800.0, 100.0), NSSize::new(400.0, 300.0));
        // SAFETY: a plain titled window, released by the test's end.
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                frame,
                NSWindowStyleMask::Titled,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        unsafe { window.setReleasedWhenClosed(false) };
        let app = AppView::new(
            mtm,
            AppOptions {
                size: (400, 300),
                ..AppOptions::default()
            },
        );
        let root = app.view();
        window.setContentView(Some(&root));
        window.makeKeyAndOrderFront(None);
        app.load(Root::of::<Outside>().into());
        let deadline = Instant::now() + Duration::from_secs(5);
        let text = loop {
            if let Some(t) = text_view(&root) {
                break t;
            }
            assert!(Instant::now() < deadline, "the input never mounted");
            spin(0.02);
        };
        window.makeFirstResponder(Some(&text));
        spin(0.1);

        let marked = NSString::from_str("n");
        let none = NSRange::new(usize::MAX >> 1, 0);
        // SAFETY: NSTextInputClient's method with a string.
        let _: () = unsafe {
            msg_send![&*text, setMarkedText: &*marked as &AnyObject, selectedRange: NSRange::new(1, 0), replacementRange: none]
        };
        // SAFETY: NSTextView's own answer, for comparison.
        let appkit: NSRect = unsafe {
            msg_send![super(&*text, NSTextView::class()), firstRectForCharacterRange: NSRange::new(0, 1), actualRange: std::ptr::null_mut::<NSRange>()]
        };
        let rect = first_rect(&text, NSRange::new(0, 1));
        let caret = first_rect(&text, NSRange::new(1, 0));
        // The input's right edge, on screen: the content's left edge, then
        // the input's frame (-650 + 600).
        let right = window.frame().origin.x - 50.0;
        assert_eq!(
            appkit,
            NSRect::ZERO,
            "AppKit answered for text out of sight; the case no longer needs one"
        );
        assert!(
            rect.size.width > 0.0 && rect.size.height > 0.0,
            "no rect for the marked text: {rect:?}"
        );
        assert!(
            (rect.origin.x + rect.size.width - right).abs() < 1.0,
            "the marked text is not at the input's right edge: {rect:?}"
        );
        assert!(
            caret.size.height > 0.0 && (caret.origin.x - right).abs() < 1.0,
            "the caret is not at the input's right edge: {caret:?}"
        );

        app.stop();
        window.close();
        println!("ime_rects: outside: ok");
    }
}
