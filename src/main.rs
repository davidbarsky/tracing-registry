#![feature(arbitrary_self_types)]

use serde_json::{json, Value};
use sharded_slab::{Guard, Slab};

use std::{
    any::Any,
    cell::RefCell,
    collections::{HashMap, HashSet},
    convert::TryInto,
    fmt, io,
    marker::PhantomData,
    sync::{Arc, Mutex},
};

use tracing::{
    error,
    field::{Field, Visit},
    info, span,
    span::Id,
    Event, Level, Metadata,
};
use tracing_core::{field::FieldSet, Interest, Subscriber};
use tracing_subscriber::{layer::Context, Layer};

pub mod context;
pub mod field;
pub mod format;
pub mod sync;
mod thread;
pub mod time;

use crate::format::{FormatEvent, FormatFields};
use sync::RwLock;

mod sealed {
    pub trait Sealed<A = ()> {}
}

/// A type that can create [`io::Write`] instances.
///
/// `MakeWriter` is used by [`FmtSubscriber`] to print formatted text representations of
/// [`Event`]s.
///
/// This trait is already implemented for function pointers and immutably-borrowing closures that
/// return an instance of [`io::Write`], such as [`io::stdout`] and [`io::stderr`].
///
/// [`io::Write`]: https://doc.rust-lang.org/std/io/trait.Write.html
/// [`FmtSubscriber`]: ../struct.Subscriber.html
/// [`Event`]: https://docs.rs/tracing-core/0.1.5/tracing_core/event/struct.Event.html
/// [`io::stdout`]: https://doc.rust-lang.org/std/io/fn.stdout.html
/// [`io::stderr`]: https://doc.rust-lang.org/std/io/fn.stderr.html
pub trait MakeWriter {
    /// The concrete [`io::Write`] implementation returned by [`make_writer`].
    ///
    /// [`io::Write`]: https://doc.rust-lang.org/std/io/trait.Write.html
    /// [`make_writer`]: #tymethod.make_writer
    type Writer: io::Write;

    /// Returns an instance of [`Writer`].
    ///
    /// # Implementer notes
    ///
    /// [`FmtSubscriber`] will call this method each time an event is recorded. Ensure any state
    /// that must be saved across writes is not lost when the [`Writer`] instance is dropped. If
    /// creating a [`io::Write`] instance is expensive, be sure to cache it when implementing
    /// [`MakeWriter`] to improve performance.
    ///
    /// [`Writer`]: #associatedtype.Writer
    /// [`FmtSubscriber`]: ../struct.Subscriber.html
    /// [`io::Write`]: https://doc.rust-lang.org/std/io/trait.Write.html
    /// [`MakeWriter`]: trait.MakeWriter.html
    fn make_writer(&self) -> Self::Writer;
}

impl<F, W> MakeWriter for F
where
    F: Fn() -> W,
    W: io::Write,
{
    type Writer = W;

    fn make_writer(&self) -> Self::Writer {
        (self)()
    }
}

pub struct ConsoleLayer<
    S = Registry,
    N = format::DefaultFields,
    E = format::Format<format::Full>,
    W = fn() -> io::Stdout,
> {
    is_interested: Box<dyn Fn(&Event<'_>) -> bool + Send + Sync + 'static>,
    inner: PhantomData<S>,
    make_writer: W,
    fmt_fields: N,
    fmt_event: E,
}

pub struct ConsoleLayerBuilder<
    S = Registry,
    N = format::DefaultFields,
    E = format::Format<format::Full>,
    W = fn() -> io::Stdout,
> {
    fmt_fields: N,
    fmt_event: E,
    make_writer: W,
    is_interested: Box<dyn Fn(&Event<'_>) -> bool + Send + Sync + 'static>,
    inner: PhantomData<S>,
}

impl ConsoleLayer {
    fn builder() -> ConsoleLayerBuilder {
        ConsoleLayerBuilder::default()
    }
}

impl<S, N, E, W> ConsoleLayerBuilder<S, N, E, W>
where
    S: Subscriber,
    N: for<'writer> FormatFields<'writer> + 'static,
    E: FormatEvent<N> + 'static,
    W: MakeWriter + 'static,
{
    fn with_interest<F>(self, f: F) -> Self
    where
        F: Fn(&Event<'_>) -> bool + Send + Sync + 'static,
    {
        Self {
            is_interested: Box::new(f),
            ..self
        }
    }
}

// this needs to be a seperate impl block because we're re-assigning the the W2 (make_writer)
// type paramater from the default.
impl<S, N, E, W> ConsoleLayerBuilder<S, N, E, W> {
    pub fn with_writer<W2>(self, make_writer: W2) -> ConsoleLayerBuilder<S, N, E, W2>
    where
        W2: MakeWriter + 'static,
    {
        ConsoleLayerBuilder {
            fmt_fields: self.fmt_fields,
            fmt_event: self.fmt_event,
            is_interested: self.is_interested,
            inner: self.inner,
            make_writer,
        }
    }
}

impl<S, N, E, W> ConsoleLayerBuilder<S, N, E, W>
where
    S: Subscriber,
    N: for<'writer> FormatFields<'writer> + 'static,
    E: FormatEvent<N> + 'static,
    W: MakeWriter + 'static,
{
    fn build(self) -> ConsoleLayer<S, N, E, W> {
        ConsoleLayer {
            is_interested: self.is_interested,
            inner: self.inner,
            make_writer: self.make_writer,
            fmt_fields: self.fmt_fields,
            fmt_event: self.fmt_event,
        }
    }
}

impl Default for ConsoleLayerBuilder {
    fn default() -> Self {
        Self {
            is_interested: Box::new(|_| true),
            inner: PhantomData,
            fmt_fields: format::DefaultFields::default(),
            fmt_event: format::Format::default(),
            make_writer: io::stdout,
        }
    }
}

// === impl Formatter ===

impl<S, N, E, W> ConsoleLayer<S, N, E, W>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'writer> FormatFields<'writer> + 'static,
    E: FormatEvent<N> + 'static,
    W: MakeWriter + 'static,
{
    #[inline]
    fn ctx(&self) -> context::Context<'_, N> {
        unimplemented!()
    }
}

impl<S, N, E, W> Layer<S> for ConsoleLayer<S, N, E, W>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'writer> FormatFields<'writer> + 'static,
    E: FormatEvent<N> + 'static,
    W: MakeWriter + 'static,
{
    fn on_close(&self, id: Id, ctx: Context<S>) {}

    fn on_event(&self, event: &Event, ctx: Context<S>) {
        thread_local! {
            static BUF: RefCell<String> = RefCell::new(String::new());
        }

        if (self.is_interested)(event) {
            BUF.with(|buf| {
                let borrow = buf.try_borrow_mut();
                let mut a;
                let mut b;
                let buf = match borrow {
                    Ok(buf) => {
                        a = buf;
                        &mut *a
                    }
                    _ => {
                        b = String::new();
                        &mut b
                    }
                };

                if self.fmt_event.format_event(&self.ctx(), buf, event).is_ok() {
                    let mut writer = self.make_writer.make_writer();
                    let _ = io::Write::write_all(&mut writer, buf.as_bytes());
                }

                buf.clear();
            });
        }
    }
}

pub trait LookupSpan<'a> {
    type Span: SpanData<'a> + fmt::Debug;
    fn span(&'a self, id: &Id) -> Option<Self::Span>;
}

pub trait SpanData<'a> {
    type Children: Iterator<Item = &'a Id>;
    type Follows: Iterator<Item = &'a Id>;

    fn id(&self) -> Id;
    fn metadata(&self) -> &'static Metadata<'static>;
    fn parent(&self) -> Option<&Id>;
    fn children(&'a self) -> Self::Children;
    fn follows_from(&'a self) -> Self::Follows;
}

struct RegistryVisitor<'a>(&'a mut HashMap<&'static str, Value>);

impl<'a> Visit for RegistryVisitor<'a> {
    // TODO: special visitors for various formats that honeycomb.io supports
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        let s = format!("{:?}", value);
        self.0.insert(field.name(), json!(s));
    }
}

impl<'a> SpanData<'a> for Guard<'a, BigSpan> {
    type Children = std::slice::Iter<'a, Id>; // not yet implemented...
    type Follows = std::slice::Iter<'a, Id>;

    fn id(&self) -> Id {
        let id: u64 = self.idx().try_into().unwrap();
        Id::from_u64(id)
    }
    fn metadata(&self) -> &'static Metadata<'static> {
        (*self).metadata
    }
    fn parent(&self) -> Option<&Id> {
        unimplemented!("david: add this to `BigSpan`")
    }
    fn children(&self) -> Self::Children {
        unimplemented!("david: add this to `BigSpan`")
    }
    fn follows_from(&self) -> Self::Follows {
        unimplemented!("david: add this to `BigSpan`")
    }
}

#[derive(Debug)]
pub struct BigSpan {
    metadata: &'static Metadata<'static>,
    values: Mutex<HashMap<&'static str, Value>>,
    events: Mutex<Vec<BigEvent>>,
}

impl BigSpan {
    pub fn name(&self) -> &'static str {
        self.metadata.name()
    }

    pub fn fields(&self) -> &FieldSet {
        self.metadata.fields()
    }

    #[inline(always)]
    fn with_parent<'registry, F, E>(
        self: Guard<'registry, BigSpan>,
        my_id: &Id,
        last_id: Option<&Id>,
        f: &mut F,
        registry: &'registry Registry,
    ) -> Result<(), E>
    where
        F: FnMut(&Id, Guard<'_, BigSpan>) -> Result<(), E>,
    {
        if let Some(parent_id) = self.parent() {
            if Some(parent_id) != last_id {
                if let Some(parent) = registry.get(parent_id) {
                    parent.with_parent(parent_id, Some(my_id), f, registry)?;
                } else {
                    panic!("missing span for {:?}; this is a bug", parent_id);
                }
            }
        }
        if let Some(span) = registry.get(my_id) {
            f(my_id, span);
        }
        Ok(())
    }
}

#[derive(Debug)]
struct BigEvent {
    parent: Id,
    metadata: &'static Metadata<'static>,
    values: HashMap<&'static str, Value>,
}

// XXX(eliza): should this have a SpanData bound? The expectation is that there
// would be add'l impls for `T: LookupSpan where T::Span: Extensions`...
//
// XXX(eliza): also, consider having `.extensions`/`.extensions_mut` methods to
// get the extensions, so we can control read locking vs write locking?
pub trait Extensions {
    fn get<T: Any>(&self) -> Option<&T>;
    fn get_mut<T: Any>(&mut self) -> Option<&mut T>;
    fn insert<T: Any>(&mut self, t: T) -> Option<T>;
    fn remove<T: Any>(&mut self) -> Option<T>;
}

#[derive(Debug)]
pub struct Registry {
    spans: Arc<Slab<BigSpan>>,
    local_spans: RwLock<SpanStack>,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            spans: Arc::new(Slab::new()),
            local_spans: RwLock::new(SpanStack::new()),
        }
    }
}
#[inline]
fn idx_to_id(idx: usize) -> Id {
    Id::from_u64(idx as u64 + 1)
}

#[inline]
fn id_to_idx(id: &Id) -> usize {
    id.into_u64() as usize - 1
}

impl Registry {
    fn insert(&self, s: BigSpan) -> Option<usize> {
        self.spans.insert(s)
    }

    fn get(&self, id: &Id) -> Option<Guard<BigSpan>> {
        self.spans.get(id_to_idx(id))
    }

    fn take(&self, id: Id) -> Option<BigSpan> {
        self.spans.take(id_to_idx(&id))
    }
}

#[derive(Debug)]
struct ContextId {
    id: Id,
    duplicate: bool,
}

#[derive(Debug)]
struct SpanStack {
    stack: Vec<ContextId>,
    ids: HashSet<Id>,
}

impl SpanStack {
    fn new() -> Self {
        SpanStack {
            stack: vec![],
            ids: HashSet::new(),
        }
    }

    fn push(&mut self, id: Id) {
        let duplicate = self.ids.contains(&id);
        if !duplicate {
            self.ids.insert(id.clone());
        }
        self.stack.push(ContextId { id, duplicate })
    }

    fn pop(&mut self, expected_id: &Id) -> Option<Id> {
        if &self.stack.last()?.id == expected_id {
            let ContextId { id, duplicate } = self.stack.pop()?;
            if !duplicate {
                self.ids.remove(&id);
            }
            Some(id)
        } else {
            None
        }
    }

    #[inline]
    fn current(&self) -> Option<&Id> {
        self.stack
            .iter()
            .rev()
            .find(|context_id| !context_id.duplicate)
            .map(|context_id| &context_id.id)
    }
}

/// Tracks the currently executing span on a per-thread basis.
#[derive(Debug)]
pub struct CurrentSpan {
    current: thread::Local<Vec<Id>>,
}

impl CurrentSpan {
    /// Returns a new `CurrentSpan`.
    pub fn new() -> Self {
        Self {
            current: thread::Local::new(),
        }
    }

    /// Returns the [`Id`](::Id) of the span in which the current thread is
    /// executing, or `None` if it is not inside of a span.
    pub fn id(&self) -> Option<Id> {
        self.current.with(|current| current.last().cloned())?
    }

    /// Records that the current thread has entered the span with the provided ID.
    pub fn enter(&self, span: Id) {
        self.current.with(|current| current.push(span));
    }

    /// Records that the current thread has exited a span.
    pub fn exit(&self) {
        self.current.with(|current| {
            let _ = current.pop();
        });
    }
}

thread_local! {
    static CONTEXT: RefCell<SpanStack> = RefCell::new(SpanStack::new());
}

impl Subscriber for Registry {
    fn register_callsite(&self, _: &'static Metadata<'static>) -> Interest {
        Interest::always()
    }

    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }

    #[inline]
    fn new_span(&self, attrs: &span::Attributes<'_>) -> span::Id {
        let mut values = HashMap::new();
        let mut visitor = RegistryVisitor(&mut values);
        attrs.record(&mut visitor);
        let s = BigSpan {
            metadata: attrs.metadata(),
            values: Mutex::new(values),
            events: Mutex::new(vec![]),
        };
        let id = (self.insert(s).expect("Unable to allocate another span") + 1) as u64;
        Id::from_u64(id.try_into().unwrap())
    }

    #[inline]
    fn record(&self, _span: &span::Id, _values: &span::Record<'_>) {
        // self.spans.record(span, values, &self.fmt_fields)
        // unimplemented!()
    }

    fn record_follows_from(&self, _span: &span::Id, _follows: &span::Id) {
        // TODO: implement this please
    }

    fn enter(&self, id: &span::Id) {
        let id = id.into_u64();
        self.local_spans
            .write()
            .expect("Mutex poisoned")
            .push(span::Id::from_u64(id));
    }

    fn event(&self, event: &Event<'_>) {
        let id = match event.parent() {
            Some(id) => Some(id.clone()),
            None => {
                if event.is_contextual() {
                    self.local_spans
                        .read()
                        .expect("Mutex poisoned")
                        .current()
                        .map(|id| id.clone())
                } else {
                    None
                }
            }
        };
        if let Some(id) = id {
            let mut values = HashMap::new();
            let mut visitor = RegistryVisitor(&mut values);
            event.record(&mut visitor);
            let span = self.get(&id).expect("Missing parent span for event");
            let event = BigEvent {
                parent: id,
                metadata: event.metadata(),
                values,
            };
            span.events.lock().expect("Mutex poisoned").push(event);
        }
    }

    fn exit(&self, id: &span::Id) {
        self.local_spans.write().expect("Mutex poisoned").pop(id);
    }

    #[inline]
    fn try_close(&self, id: span::Id) -> bool {
        let _ = self.take(id);
        true
    }
}

impl<'a> LookupSpan<'a> for Registry {
    type Span = Guard<'a, BigSpan>;

    fn span(&'a self, id: &Id) -> Option<Self::Span> {
        self.get(id)
    }
}

fn main() {
    let stderr = ConsoleLayer::builder()
        .with_interest(|event| event.metadata().level() >= &Level::WARN)
        .with_writer(io::stderr)
        .build();

    let stdout = ConsoleLayer::builder()
        .with_interest(|event| event.metadata().level() == &Level::INFO)
        .with_writer(io::stdout)
        .build();

    let subscriber = stderr.and_then(stdout).with_subscriber(Registry::default());
    tracing::subscriber::set_global_default(subscriber).expect("Could not set global default");

    let span = span!(Level::INFO, "my_loop");
    let _entered = span.enter();
    for i in 0..2 {
        error!("Closing!");
        span!(Level::INFO, "iteration").in_scope(|| info!(iteration = i, "In a span!"));
    }
}
