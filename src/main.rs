use serde_json::{json, Value};
use sharded_slab::{Guard, Slab};
use std::any::Any;
use std::collections::HashMap;
use std::convert::TryInto;
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tracing::field::{Field, Visit};
use tracing::span::Id;
use tracing::{info, span, Event, Level, Metadata};
use tracing_core::{Interest, Subscriber};

pub trait LookupSpan {
    type Span: SpanData;
    fn span(&self, id: &Id) -> Option<Self::Span>;
}

pub trait SpanData {
    type Children: Iterator<Item = Id>;
    type Follows: Iterator<Item = Id>;

    fn id(&self) -> Option<Id>;
    fn metadata(&self) -> &'static Metadata<'static>;
    fn parent(&self) -> Option<Id>;
    fn children(&self) -> Self::Children;
    fn follows_from(&self) -> Self::Follows;
}

struct RegistryVisitor<'a>(&'a mut HashMap<&'static str, Value>);

impl<'a> Visit for RegistryVisitor<'a> {
    // TODO: special visitors for various formats that honeycomb.io supports
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        let s = format!("{:?}", value);
        self.0.insert(field.name(), json!(s));
    }
}

#[derive(Debug)]
struct BigSpan {
    metadata: &'static Metadata<'static>,
    values: HashMap<&'static str, Value>,
    events: Vec<BigEvent>,
}

#[derive(Debug)]
struct BigEvent {
    parent: usize,
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
    spans: Arc<Slab<Mutex<BigSpan>>>,
    events: Arc<Slab<BigEvent>>,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            spans: Arc::new(Slab::new()),
            events: Arc::new(Slab::new()),
        }
    }
}

impl Registry {
    fn insert(&self, s: BigSpan) -> Option<usize> {
        self.spans.insert(Mutex::new(s))
    }

    fn get(&self, id: usize) -> Option<Guard<Mutex<BigSpan>>> {
        self.spans.get(id)
    }

    /// if `true` is returned, then the span was successfully removed.
    fn remove(&self, id: usize) -> bool {
        self.spans.remove(id)
    }
}

static CURRENT_SPAN: AtomicUsize = AtomicUsize::new(0);
impl Subscriber for Registry {
    fn register_callsite(&self, _: &'static Metadata<'static>) -> Interest {
        Interest::always()
    }

    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        true
    }

    #[inline]
    fn new_span(&self, attrs: &span::Attributes<'_>) -> span::Id {
        let mut values = HashMap::new();
        let mut visitor = RegistryVisitor(&mut values);
        attrs.record(&mut visitor);
        let s = BigSpan {
            metadata: attrs.metadata(),
            values: values,
            events: vec![],
        };
        let id = self.insert(s).expect("Unable to allocate another span") + 1;
        CURRENT_SPAN.swap(id, Ordering::SeqCst);
        Id::from_u64(id.try_into().unwrap())
    }

    #[inline]
    fn record(&self, span: &span::Id, values: &span::Record<'_>) {
        // self.spans.record(span, values, &self.fmt_fields)
        // unimplemented!()
    }

    fn record_follows_from(&self, _span: &span::Id, _follows: &span::Id) {
        // TODO: implement this please
    }

    fn enter(&self, id: &span::Id) {
        // self.spans.push(id);
        // unimplemented!()
    }

    fn event(&self, event: &Event<'_>) {
        let id = match event.parent() {
            Some(id) => Some(id.clone()),
            None => {
                if event.is_contextual() {
                    let id = CURRENT_SPAN.load(Ordering::SeqCst);
                    Some(span::Id::from_u64(id.try_into().unwrap()))
                } else {
                    None
                }
            }
        };
        if let Some(id) = id {
            let mut values = HashMap::new();
            let mut visitor = RegistryVisitor(&mut values);
            event.record(&mut visitor);
            let id: usize = id.into_u64().try_into().unwrap();
            let id = id - 1;
            let span = self.get(id).expect("Missing parent span for event");
            let mut span = span.lock().expect("Mutex poisoned");
            let event = BigEvent {
                parent: id,
                metadata: event.metadata(),
                values,
            };
            span.events.push(event);
        }
    }

    fn exit(&self, id: &span::Id) {
        let id: usize = id.into_u64().try_into().unwrap();
        let id = id - 1;
        let span = self.spans.get(id);
        dbg!(span);
    }

    #[inline]
    fn try_close(&self, id: span::Id) -> bool {
        self.spans.remove(id.into_u64() as usize)
    }
}

fn main() {
    // let registry = Registry::default();

    // let key = registry.insert(String::from("hello world")).unwrap();
    // let hello = registry.get(key).expect("item missing");
    // let mut hello = hello.lock().expect("mutex poisoned");
    // *hello = String::from("hello, world!");
    // drop(hello);

    // dbg!(&registry.get(key));
    tracing::subscriber::set_global_default(Registry::default())
        .expect("Could not set global default");

    let span = span!(Level::INFO, "my_loop");
    let _entered = span.enter();
    for i in 0..10 {
        info!(iteration = i, "In a span!");
    }
}
