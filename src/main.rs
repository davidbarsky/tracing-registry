use serde_json::{json, Value};
use sharded_slab::{Guard, Slab};

use std::any::Any;
use std::cell::Cell;
use std::collections::HashMap;
use std::convert::TryInto;
use std::fmt;
use std::io;
use std::sync::{Arc, Mutex};

use std::marker::PhantomData;
use tracing::field::{Field, Visit};
use tracing::span::Id;
use tracing::{error, info, span, Event, Level, Metadata};
use tracing_core::{Interest, Subscriber};
use tracing_subscriber::{layer::Context, Layer};

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

pub struct ConsoleLayer<S: Subscriber, W>
where
    W: MakeWriter + Send + Sync + 'static,
{
    is_interested: Box<dyn Fn(&Event<'_>) -> bool + Send + Sync + 'static>,
    make_writer: W,
    inner: PhantomData<S>,
}

pub struct ConsoleLayerBuilder<S, W = fn() -> io::Stdout>
where
    S: Subscriber,
    W: MakeWriter + Send + Sync + 'static,
{
    is_interested: Box<dyn Fn(&Event<'_>) -> bool + Send + Sync + 'static>,
    make_writer: W,
    inner: PhantomData<S>,
}

impl<S, W> ConsoleLayer<S, W>
where
    S: Subscriber,
    W: MakeWriter + Send + Sync + 'static,
{
    fn builder() -> ConsoleLayerBuilder<S> {
        ConsoleLayerBuilder::default()
    }
}

impl<S, W> ConsoleLayerBuilder<S, W>
where
    S: Subscriber,
    W: MakeWriter + Send + Sync + 'static,
{
    fn with_interest<F>(&mut self, f: F) -> &mut Self
    where
        F: Fn(&Event<'_>) -> bool + Send + Sync + 'static,
    {
        self.is_interested = Box::new(f);
        self
    }

    fn with_writer(&mut self, w: W) -> &mut Self {
        self.make_writer = w;
        self
    }

    fn build(self) -> ConsoleLayer<S, W> {
        ConsoleLayer {
            is_interested: self.is_interested,
            make_writer: self.make_writer,
            inner: self.inner,
        }
    }
}

impl<S> Default for ConsoleLayerBuilder<S>
where
    S: Subscriber,
{
    fn default() -> Self {
        Self {
            is_interested: Box::new(|_| true),
            inner: PhantomData,
            make_writer: io::stdout,
        }
    }
}

impl<S, W> Layer<S> for ConsoleLayer<S, W>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    W: MakeWriter + Send + Sync + 'static,
{
    fn on_close(&self, id: Id, ctx: Context<S>) {}

    fn on_event(&self, event: &Event, ctx: Context<S>) {
        if (self.is_interested)(event) {}
    }
}

pub trait LookupSpan<'a> {
    type Span: SpanData<'a> + fmt::Debug;
    fn span(&'a self, id: &Id) -> Option<Self::Span>;
}

pub trait SpanData<'a> {
    type Children: Iterator<Item = &'a Id>;
    type Follows: Iterator<Item = &'a Id>;

    fn id(&self) -> &Id;
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

    fn id(&self) -> &Id {
        unimplemented!("david: add this to `BigSpan`")
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
struct Registry {
    spans: Arc<Slab<BigSpan>>,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            spans: Arc::new(Slab::new()),
        }
    }
}

fn convert_id(id: Id) -> usize {
    let id: usize = id.into_u64().try_into().unwrap();
    id - 1
}

impl Registry {
    fn insert(&self, s: BigSpan) -> Option<usize> {
        self.spans.insert(s)
    }

    fn get(&self, id: &Id) -> Option<Guard<BigSpan>> {
        self.spans.get(convert_id(id.clone()))
    }

    fn take(&self, id: Id) -> Option<BigSpan> {
        let id = convert_id(id);
        self.spans.take(id)
    }
}

thread_local! {
    pub static CURRENT_SPAN: Cell<Option<u64>> = Cell::new(Some(1));
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
        CURRENT_SPAN.with(|s| s.set(Some(id)));
    }

    fn event(&self, event: &Event<'_>) {
        let id = match event.parent() {
            Some(id) => Some(id.clone()),
            None => {
                if event.is_contextual() {
                    let id = CURRENT_SPAN.with(|s| s.get());
                    id.map(span::Id::from_u64)
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

    fn exit(&self, _id: &span::Id) {
        CURRENT_SPAN.with(|s| s.take());
    }

    #[inline]
    fn try_close(&self, id: span::Id) -> bool {
        let span = self.take(id);
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
    // let stderr = ConsoleLayer::builder()
    //     .with_interest(|event| event.metadata().level() == &Level::ERROR)
    //     .with_writer(io::stdout)
    //     .build();

    let stdout = ConsoleLayer::builder()
        .with_interest(|event| event.metadata().level() == &Level::INFO)
        .with_writer(io::stdout)
        .build();

    let subscriber = stdout.with_subscriber(Registry::default());
    tracing::subscriber::set_global_default(subscriber).expect("Could not set global default");

    let span = span!(Level::INFO, "my_loop");
    let _entered = span.enter();
    for i in 0..2 {
        error!("Closing!");
        span!(Level::INFO, "iteration").in_scope(|| info!(iteration = i, "In a span!"));
    }
}
