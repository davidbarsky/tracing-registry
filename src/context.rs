use super::format::FormatFields;
use crate::{BigSpan, Registry};
use sharded_slab::{Guard, Slab};
use std::{cell::RefCell, collections::HashSet, fmt};
pub(crate) use tracing_core::span::Id;

#[cfg(feature = "json")]
use serde::{ser::SerializeStruct, Serialize, Serializer};

// pub struct Span<'a> {
//     lock: OwningHandle<RwLockReadGuard<'a, Slab>, RwLockReadGuard<'a, Slot>>,
// }

/// Represents the `Subscriber`'s view of the current span context to a
/// formatter.
#[derive(Debug)]
pub struct Context<'a, F> {
    registry: &'a Registry,
    fmt_fields: &'a F,
}

struct ContextId {
    id: Id,
    duplicate: bool,
}

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

thread_local! {
    static CONTEXT: RefCell<SpanStack> = RefCell::new(SpanStack::new());
}

macro_rules! debug_panic {
    ($($args:tt)*) => {
        #[cfg(debug_assertions)] {
            if !std::thread::panicking() {
                panic!($($args)*)
            }
        }
    }
}

// ===== impl Span =====

// impl<'a> Span<'a> {
//     pub fn name(&self) -> &'static str {
//         unimplemented!()
//     }

//     pub fn metadata(&self) -> &'static Metadata<'static> {
//         unimplemented!()
//     }

//     pub fn fields(&self) -> &str {
//         self.lock.fields.as_ref()
//     }

//     pub fn parent(&self) -> Option<&Id> {
//         unimplemented!()
//     }

//     #[inline(always)]
//     fn with_parent<'registry, F, E>(
//         self,
//         my_id: &Id,
//         last_id: Option<&Id>,
//         f: &mut F,
//         registry: &'registry Registry,
//     ) -> Result<(), E>
//     where
//         F: FnMut(&Id, Span<'_>) -> Result<(), E>,
//     {
//         if let Some(parent_id) = self.parent() {
//             if Some(parent_id) != last_id {
//                 if let Some(parent) = registry.get(parent_id) {
//                     parent.with_parent(parent_id, Some(my_id), f, registry)?;
//                 } else {
//                     debug_panic!("missing span for {:?}; this is a bug", parent_id);
//                 }
//             }
//         }
//         f(my_id, self)
//     }
// }

// impl<'a> fmt::Debug for Span<'a> {
//     fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
//         f.debug_struct("Span")
//             .field("name", &self.name())
//             .field("parent", &self.parent())
//             .field("metadata", self.metadata())
//             .field("fields", &self.fields())
//             .finish()
//     }
// }

// #[cfg(feature = "json")]
// impl<'a> Serialize for Span<'a> {
//     fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
//     where
//         S: Serializer,
//     {
//         let mut serializer = serializer.serialize_struct("Span", 2)?;

//         serializer.serialize_field("name", self.name())?;
//         serializer.serialize_field("fields", self.fields())?;
//         serializer.end()
//     }
// }

// ===== impl Context =====

impl<'a, F> Context<'a, F> {
    /// Applies a function to each span in the current trace context.
    ///
    /// The function is applied in order, beginning with the root of the trace,
    /// and ending with the current span. If the function returns an error,
    /// this will short-circuit.
    ///
    /// If invoked from outside of a span, the function will not be applied.
    ///
    /// Note that if we are currently unwinding, this will do nothing, rather
    /// than potentially causing a double panic.
    pub fn visit_spans<N, E>(&self, mut f: N) -> Result<(), E>
    where
        N: FnMut(&Id, Guard<'_, BigSpan>) -> Result<(), E>,
    {
        CONTEXT
            .try_with(|current| {
                if let Some(id) = current.borrow().current() {
                    if let Some(span) = self.registry.get(id) {
                        // with_parent uses the call stack to visit the span
                        // stack in reverse order, without having to allocate
                        // a buffer.
                        return span.with_parent(id, None, &mut f, self.registry);
                    } else {
                        debug_panic!("missing span for {:?}; this is a bug", id);
                    }
                }
                Ok(())
            })
            .unwrap_or(Ok(()))
    }

    /// Executes a closure with the reference to the current span.
    pub fn with_current<N, R>(&self, f: N) -> Option<R>
    where
        N: FnOnce((&Id, Guard<'_, BigSpan>)) -> R,
    {
        // If the lock is poisoned or the thread local has already been
        // destroyed, we might be in the middle of unwinding, so this
        // will just do nothing rather than cause a double panic.
        CONTEXT
            .try_with(|current| {
                if let Some(id) = current.borrow().current() {
                    if let Some(span) = self.registry.get(&id) {
                        return Some(f((&id, span)));
                    } else {
                        debug_panic!("missing span for {:?}, this is a bug", id);
                    }
                }
                None
            })
            .ok()?
    }

    pub(crate) fn new(registry: &'a Registry, fmt_fields: &'a F) -> Self {
        Self {
            registry,
            fmt_fields,
        }
    }
}

impl<'ctx, 'writer, F> FormatFields<'writer> for Context<'ctx, F>
where
    F: FormatFields<'writer>,
{
    #[inline]
    fn format_fields<R>(&self, writer: &'writer mut dyn fmt::Write, fields: R) -> fmt::Result
    where
        R: crate::field::RecordFields,
    {
        self.fmt_fields.format_fields(writer, fields)
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
