//! macOS native chat vendor — the AppKit conversation view inside the menu-bar
//! popover ([`super::macos_popover`]).
//!
//! A hand-built iMessage-style view: an `NSScrollView` of rounded message bubbles
//! (a vertical `NSStackView` in a flipped document view) above a composer — a rounded
//! pill with the text input and an inline send button. No attach button; you type and
//! send. It is a pure view: it holds no agent state. The
//! [`crate::body::capabilities::chat`] bridge feeds it (history + the live echoes) via
//! the delivery functions below, and it reports a sent line back through the registered
//! send handler — so this file never reaches up into the body layer.
//!
//! AppKit objects live on the process main thread and are leaked for the process
//! lifetime (like the tray); the bridge's cross-thread deliveries hop onto the main run
//! loop via `performSelectorOnMainThread:`. Only compiled on macOS.

use std::cell::RefCell;
use std::sync::OnceLock;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, Sel};
use objc2::{DefinedClass, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSBezelStyle, NSBezierPath, NSBox, NSBoxType, NSButton, NSColor,
    NSFont, NSLayoutDimension, NSLayoutXAxisAnchor, NSLayoutYAxisAnchor, NSLineBreakMode,
    NSScrollView, NSStackView, NSTextField, NSTitlePosition, NSUserInterfaceLayoutOrientation,
    NSView,
};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize, NSString};

use super::macos_popover::{POPOVER_H, POPOVER_W};

// ---------------------------------------------------------------------------
// Layout + palette
// ---------------------------------------------------------------------------

const COMPOSER_H: f64 = 54.0;
const PAD: f64 = 10.0;
const BUBBLE_MAX_W: f64 = 276.0;
const TEXT_MAX_W: f64 = 250.0;

fn in_bg() -> Retained<NSColor> {
    NSColor::colorWithSRGBRed_green_blue_alpha(0.075, 0.655, 0.961, 1.0) // #13a7f5
}
fn out_bg() -> Retained<NSColor> {
    NSColor::colorWithSRGBRed_green_blue_alpha(0.925, 0.933, 0.945, 1.0) // #eceef1
}
fn ink() -> Retained<NSColor> {
    NSColor::colorWithSRGBRed_green_blue_alpha(0.082, 0.094, 0.110, 1.0) // #15181c
}
fn line() -> Retained<NSColor> {
    NSColor::colorWithSRGBRed_green_blue_alpha(0.0, 0.0, 0.0, 0.16)
}

// ---------------------------------------------------------------------------
// Handlers registered by the bridge (this vendor never imports the body layer)
// ---------------------------------------------------------------------------

type SendHandler = Box<dyn Fn(String) + Send + Sync>;
type OpenHandler = Box<dyn Fn() + Send + Sync>;

static SEND: OnceLock<SendHandler> = OnceLock::new();
static OPEN: OnceLock<OpenHandler> = OnceLock::new();

/// Register the bridge's callbacks: `on_send(text)` dispatches a typed line, `on_open`
/// seeds history + starts the live feed on first open. Called once at startup.
pub fn set_handlers(on_send: SendHandler, on_open: OpenHandler) {
    let _ = SEND.set(on_send);
    let _ = OPEN.set(on_open);
}

/// The popover became visible — let the bridge seed/feed (it guards re-opens).
pub fn notify_opened() {
    if let Some(f) = OPEN.get() {
        f();
    }
}

// ---------------------------------------------------------------------------
// A rounded bubble background (custom-drawn so we need no CALayer/CGColor)
// ---------------------------------------------------------------------------

struct BubbleIvars {
    color: Retained<NSColor>,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "HiAgentBubble"]
    #[ivars = BubbleIvars]
    struct Bubble;

    unsafe impl NSObjectProtocol for Bubble {}

    impl Bubble {
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            let b = self.bounds();
            let r = (b.size.height / 2.0).min(16.0);
            let path = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(b, r, r);
            self.ivars().color.set();
            path.fill();
        }
    }
);

impl Bubble {
    fn new(mtm: MainThreadMarker, color: Retained<NSColor>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(BubbleIvars { color });
        unsafe { msg_send![super(this), init] }
    }
}

// ---------------------------------------------------------------------------
// A flipped container so the message stack grows top → bottom
// ---------------------------------------------------------------------------

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "HiAgentFlipped"]
    struct Flipped;

    unsafe impl NSObjectProtocol for Flipped {}

    impl Flipped {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }
    }
);

// ---------------------------------------------------------------------------
// The chat controller — owns the view tree + the in-progress bubble refs
// ---------------------------------------------------------------------------

struct ChatIvars {
    scroll: Retained<NSScrollView>,
    doc: Retained<Flipped>,
    list: Retained<NSStackView>,
    field: Retained<NSTextField>,
    /// The rolling live-transcript "in" bubble's label, while one is in flight.
    partial_in: RefCell<Option<Retained<NSTextField>>>,
    /// The streaming agent-reply "out" bubble's label, while a reply is in flight.
    out_label: RefCell<Option<Retained<NSTextField>>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "HiAgentChat"]
    #[ivars = ChatIvars]
    struct Chat;

    unsafe impl NSObjectProtocol for Chat {}

    impl Chat {
        /// A settled inbound line (history, or the echo of a sent/spoken line).
        #[unsafe(method(appendIn:))]
        fn append_in(&self, text: &NSString) {
            self.add_bubble(&text.to_string(), true);
            self.scroll_to_bottom();
        }

        /// A settled agent line (history).
        #[unsafe(method(appendOut:))]
        fn append_out(&self, text: &NSString) {
            self.add_bubble(&text.to_string(), false);
            self.scroll_to_bottom();
        }

        /// Update (or open) the rolling live-transcript bubble.
        #[unsafe(method(partialIn:))]
        fn partial_in(&self, text: &NSString) {
            let s = text.to_string();
            let existing = self.ivars().partial_in.borrow().clone();
            match existing {
                Some(label) => label.setStringValue(&NSString::from_str(&s)),
                None => {
                    let label = self.add_bubble(&s, true);
                    *self.ivars().partial_in.borrow_mut() = Some(label);
                }
            }
            self.scroll_to_bottom();
        }

        /// Settle the rolling bubble (or add a fresh settled one).
        #[unsafe(method(inFinal:))]
        fn in_final(&self, text: &NSString) {
            let s = text.to_string();
            match self.ivars().partial_in.borrow_mut().take() {
                Some(label) => label.setStringValue(&NSString::from_str(&s)),
                None => {
                    self.add_bubble(&s, true);
                }
            }
            self.scroll_to_bottom();
        }

        /// Set the current agent-reply bubble to the given text (the client sends the
        /// growing reply so far), opening a bubble on the first call of an utterance.
        #[unsafe(method(agentSet:))]
        fn agent_set(&self, text: &NSString) {
            let s = text.to_string();
            let existing = self.ivars().out_label.borrow().clone();
            match existing {
                Some(label) => label.setStringValue(&NSString::from_str(&s)),
                None => {
                    let label = self.add_bubble(&s, false);
                    *self.ivars().out_label.borrow_mut() = Some(label);
                }
            }
            self.scroll_to_bottom();
        }

        /// End of the agent reply — the next reply starts a fresh bubble.
        #[unsafe(method(agentEnd))]
        fn agent_end(&self) {
            self.ivars().out_label.borrow_mut().take();
        }

        /// Send the current input (Enter or the ↑ button). The line renders from its
        /// own echo, so it isn't added locally; the field is cleared.
        #[unsafe(method(send:))]
        fn send(&self, _sender: Option<&AnyObject>) {
            let text = self.ivars().field.stringValue().to_string();
            if text.trim().is_empty() {
                return;
            }
            self.ivars().field.setStringValue(&NSString::from_str(""));
            if let Some(f) = SEND.get() {
                f(text);
            }
        }

        /// Put the keyboard in the input field.
        #[unsafe(method(focusInput))]
        fn focus_input(&self) {
            let field = &self.ivars().field;
            if let Some(window) = field.window() {
                window.makeFirstResponder(Some(field));
            }
        }
    }
);

impl Chat {
    /// Build and add a bubble row, returning its label (so streaming/rolling updates
    /// can rewrite it in place). `is_in` → trailing + blue; else → leading + grey.
    fn add_bubble(&self, text: &str, is_in: bool) -> Retained<NSTextField> {
        let mtm = MainThreadMarker::new().expect("chat runs on the main thread");
        let iv = self.ivars();
        let (bg, fg) = if is_in { (in_bg(), white()) } else { (out_bg(), ink()) };

        let label = NSTextField::labelWithString(&NSString::from_str(text), mtm);
        label.setTranslatesAutoresizingMaskIntoConstraints(false);
        label.setEditable(false);
        label.setSelectable(true);
        label.setBezeled(false);
        label.setDrawsBackground(false);
        label.setTextColor(Some(&fg));
        label.setFont(Some(&NSFont::systemFontOfSize(14.0)));
        label.setMaximumNumberOfLines(0);
        label.setLineBreakMode(NSLineBreakMode::ByWordWrapping);
        label.setPreferredMaxLayoutWidth(TEXT_MAX_W);

        let bubble = Bubble::new(mtm, bg);
        let row: Retained<NSView> = unsafe { msg_send![NSView::alloc(mtm), init] };
        bubble.setTranslatesAutoresizingMaskIntoConstraints(false);
        row.setTranslatesAutoresizingMaskIntoConstraints(false);
        bubble.addSubview(&label);
        row.addSubview(&bubble);

        // label inside bubble (padding)
        pin_x(label.leadingAnchor(), bubble.leadingAnchor(), 12.0);
        pin_x(bubble.trailingAnchor(), label.trailingAnchor(), 12.0);
        pin_y(label.topAnchor(), bubble.topAnchor(), 7.0);
        pin_y(bubble.bottomAnchor(), label.bottomAnchor(), 7.0);

        // bubble fills row vertically, capped width, aligned to one side
        same_y(bubble.topAnchor(), row.topAnchor());
        same_y(bubble.bottomAnchor(), row.bottomAnchor());
        le_const(bubble.widthAnchor(), BUBBLE_MAX_W);
        if is_in {
            same_x(bubble.trailingAnchor(), row.trailingAnchor());
            ge_x(bubble.leadingAnchor(), row.leadingAnchor());
        } else {
            same_x(bubble.leadingAnchor(), row.leadingAnchor());
            le_x(bubble.trailingAnchor(), row.trailingAnchor());
        }

        // row spans the full list width
        same_dim(row.widthAnchor(), iv.doc.widthAnchor());

        iv.list.addArrangedSubview(&row);
        label
    }

    /// Scroll the newest content into view.
    fn scroll_to_bottom(&self) {
        let iv = self.ivars();
        iv.doc.layoutSubtreeIfNeeded();
        let clip = iv.scroll.contentView();
        let doc_h = iv.doc.frame().size.height;
        let clip_h = clip.bounds().size.height;
        let y = (doc_h - clip_h).max(0.0);
        clip.setBoundsOrigin(NSPoint::new(0.0, y));
        iv.scroll.reflectScrolledClipView(&clip);
    }
}

fn white() -> Retained<NSColor> {
    NSColor::whiteColor()
}

// ---------------------------------------------------------------------------
// Constraint helpers (keep `add_bubble` readable). One activated constraint each.
// ---------------------------------------------------------------------------

fn pin_x(a: Retained<NSLayoutXAxisAnchor>, b: Retained<NSLayoutXAxisAnchor>, c: f64) {
    a.constraintEqualToAnchor_constant(&b, c).setActive(true);
}
fn pin_y(a: Retained<NSLayoutYAxisAnchor>, b: Retained<NSLayoutYAxisAnchor>, c: f64) {
    a.constraintEqualToAnchor_constant(&b, c).setActive(true);
}
fn same_x(a: Retained<NSLayoutXAxisAnchor>, b: Retained<NSLayoutXAxisAnchor>) {
    a.constraintEqualToAnchor(&b).setActive(true);
}
fn same_y(a: Retained<NSLayoutYAxisAnchor>, b: Retained<NSLayoutYAxisAnchor>) {
    a.constraintEqualToAnchor(&b).setActive(true);
}
fn ge_x(a: Retained<NSLayoutXAxisAnchor>, b: Retained<NSLayoutXAxisAnchor>) {
    a.constraintGreaterThanOrEqualToAnchor(&b).setActive(true);
}
fn le_x(a: Retained<NSLayoutXAxisAnchor>, b: Retained<NSLayoutXAxisAnchor>) {
    a.constraintLessThanOrEqualToAnchor(&b).setActive(true);
}
fn le_const(a: Retained<NSLayoutDimension>, c: f64) {
    a.constraintLessThanOrEqualToConstant(c).setActive(true);
}
fn same_dim(a: Retained<NSLayoutDimension>, b: Retained<NSLayoutDimension>) {
    a.constraintEqualToAnchor(&b).setActive(true);
}

// ---------------------------------------------------------------------------
// The leaked controller pointer + the build entry
// ---------------------------------------------------------------------------

struct ChatPtr(*const Chat);
unsafe impl Send for ChatPtr {}
unsafe impl Sync for ChatPtr {}

static CHAT: OnceLock<ChatPtr> = OnceLock::new();

/// Build the chat view tree and remember the controller, returning the root view for
/// the popover's content view controller. Called once on the main thread.
pub fn make_view(mtm: MainThreadMarker) -> Retained<NSView> {
    let root: Retained<NSView> = unsafe {
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(POPOVER_W, POPOVER_H));
        msg_send![NSView::alloc(mtm), initWithFrame: frame]
    };

    // Message list: a vertical stack inside a flipped document inside a scroll view.
    let list = NSStackView::new(mtm);
    let doc: Retained<Flipped> = unsafe { msg_send![Flipped::alloc(mtm), init] };
    let scroll = NSScrollView::new(mtm);

    list.setOrientation(NSUserInterfaceLayoutOrientation::Vertical);
    list.setSpacing(6.0);
    list.setTranslatesAutoresizingMaskIntoConstraints(false);
    doc.setTranslatesAutoresizingMaskIntoConstraints(false);
    doc.addSubview(&list);
    // list fills the flipped doc (top-anchored growth)
    same_x(list.leadingAnchor(), doc.leadingAnchor());
    same_x(list.trailingAnchor(), doc.trailingAnchor());
    same_y(list.topAnchor(), doc.topAnchor());
    same_y(doc.bottomAnchor(), list.bottomAnchor());

    scroll.setHasVerticalScroller(true);
    scroll.setDrawsBackground(false);
    scroll.setAutohidesScrollers(true);
    scroll.setDocumentView(Some(&doc));
    // doc width tracks the visible clip so bubbles get a stable wrap column
    let clip = scroll.contentView();
    same_dim(doc.widthAnchor(), clip.widthAnchor());

    scroll.setFrame(NSRect::new(
        NSPoint::new(0.0, COMPOSER_H),
        NSSize::new(POPOVER_W, POPOVER_H - COMPOSER_H),
    ));
    scroll.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );
    root.addSubview(&scroll);

    // Composer: a rounded pill (NSBox) with the input field + inline ↑ send button.
    let field = NSTextField::new(mtm);
    let send = NSButton::new(mtm);
    unsafe {
        let bar_w = POPOVER_W - 2.0 * PAD;
        let pill_h = COMPOSER_H - 16.0;
        let pill: Retained<NSBox> = msg_send![NSBox::alloc(mtm), init];
        pill.setBoxType(NSBoxType::Custom);
        pill.setTitlePosition(NSTitlePosition::NoTitle);
        pill.setCornerRadius((pill_h / 2.0).min(18.0));
        pill.setBorderWidth(1.0);
        pill.setBorderColor(&line());
        pill.setFillColor(&NSColor::whiteColor());
        pill.setContentViewMargins(NSSize::new(0.0, 0.0));
        pill.setFrame(NSRect::new(NSPoint::new(PAD, 8.0), NSSize::new(bar_w, pill_h)));
        pill.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewMaxYMargin,
        );

        let content = pill.contentView().expect("NSBox has a content view");
        let btn = 28.0;

        field.setBordered(false);
        field.setBezeled(false);
        field.setDrawsBackground(false);
        field.setFont(Some(&NSFont::systemFontOfSize(14.0)));
        field.setTextColor(Some(&ink()));
        field.setPlaceholderString(Some(&NSString::from_str("发消息…")));
        field.setFrame(NSRect::new(
            NSPoint::new(12.0, (pill_h - 22.0) / 2.0),
            NSSize::new(bar_w - 12.0 - btn - 12.0, 22.0),
        ));
        field.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        content.addSubview(&field);

        send.setTitle(&NSString::from_str("↑"));
        send.setBezelStyle(NSBezelStyle::Circular);
        send.setFrame(NSRect::new(
            NSPoint::new(bar_w - btn - 6.0, (pill_h - btn) / 2.0),
            NSSize::new(btn, btn),
        ));
        send.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinXMargin);
        content.addSubview(&send);

        root.addSubview(&pill);
        std::mem::forget(pill);
    }

    // The controller owns the refs and wires the field/button actions.
    let chat = Chat::alloc(mtm).set_ivars(ChatIvars {
        scroll,
        doc,
        list,
        field,
        partial_in: RefCell::new(None),
        out_label: RefCell::new(None),
    });
    let chat: Retained<Chat> = unsafe { msg_send![super(chat), init] };
    unsafe {
        let iv = chat.ivars();
        iv.field.setTarget(Some(&*chat));
        iv.field.setAction(Some(sel!(send:)));
        send.setTarget(Some(&*chat));
        send.setAction(Some(sel!(send:)));
    }
    let ptr: *const Chat = &*chat;
    std::mem::forget(chat);
    std::mem::forget(send);
    let _ = CHAT.set(ChatPtr(ptr));

    root
}

// ---------------------------------------------------------------------------
// Delivery — called by the bridge from the tokio thread, hopped to main
// ---------------------------------------------------------------------------

pub fn append_in(text: &str) {
    hop_str(sel!(appendIn:), text);
}
pub fn append_out(text: &str) {
    hop_str(sel!(appendOut:), text);
}
pub fn partial_in(text: &str) {
    hop_str(sel!(partialIn:), text);
}
pub fn in_final(text: &str) {
    hop_str(sel!(inFinal:), text);
}
pub fn agent_set(text: &str) {
    hop_str(sel!(agentSet:), text);
}
pub fn agent_end() {
    hop_nil(sel!(agentEnd));
}
pub fn focus_input() {
    hop_nil(sel!(focusInput));
}

fn hop_str(selector: Sel, text: &str) {
    let Some(chat) = CHAT.get() else { return };
    let ns = NSString::from_str(text);
    unsafe {
        let obj: &Chat = &*chat.0;
        let _: () = msg_send![
            obj,
            performSelectorOnMainThread: selector,
            withObject: &*ns,
            waitUntilDone: false
        ];
    }
}

fn hop_nil(selector: Sel) {
    let Some(chat) = CHAT.get() else { return };
    unsafe {
        let obj: &Chat = &*chat.0;
        let _: () = msg_send![
            obj,
            performSelectorOnMainThread: selector,
            withObject: core::ptr::null_mut::<AnyObject>(),
            waitUntilDone: false
        ];
    }
}
