use {
    alloc::{boxed::Box, sync::Arc, vec::Vec},
    core::{
        fmt::Debug,
        future::Future,
        pin::Pin,
        sync::atomic::*,
        task::{Context, Poll},
    },
    downcast_rs::{impl_downcast, DowncastSync},
    spin::Mutex,
};

pub use {super::*, handle::*, rights::*, signal::*};

mod handle;
mod rights;
mod signal;

pub trait KernelObject: DowncastSync + Debug {
    fn id(&self) -> KoID;
    fn type_name(&self) -> &'static str;
    fn signal(&self) -> Signal;
    fn add_signal_callback(&self, callback: SignalHandler);
}

impl_downcast!(sync KernelObject);

/// The base struct of a kernel object.
pub struct KObjectBase {
    pub id: KoID,
    inner: Mutex<KObjectBaseInner>,
}

/// The mutable part of `KObjectBase`.
#[derive(Default)]
struct KObjectBaseInner {
    signal: Signal,
    signal_callbacks: Vec<SignalHandler>,
}

impl Default for KObjectBase {
    fn default() -> Self {
        KObjectBase {
            id: Self::new_koid(),
            inner: Default::default(),
        }
    }
}

impl KObjectBase {
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a kernel object base with initial `signal`.
    pub fn with_signal(signal: Signal) -> Self {
        KObjectBase {
            id: Self::new_koid(),
            inner: Mutex::new(KObjectBaseInner {
                signal,
                signal_callbacks: Vec::new(),
            }),
        }
    }

    /// Generate a new KoID.
    fn new_koid() -> KoID {
        static KOID: AtomicU64 = AtomicU64::new(1024);
        KOID.fetch_add(1, Ordering::SeqCst)
    }

    /// Get the signal status.
    pub fn signal(&self) -> Signal {
        self.inner.lock().signal
    }

    /// Change signal status: first `clear` then `set` indicated bits.
    ///
    /// All signal callbacks will be called.
    pub fn signal_change(&self, clear: Signal, set: Signal) {
        let mut inner = self.inner.lock();
        let old_signal = inner.signal;
        inner.signal.remove(clear);
        inner.signal.insert(set);
        let new_signal = inner.signal;
        if new_signal == old_signal {
            return;
        }
        inner.signal_callbacks.retain(|f| !f(new_signal));
    }

    pub fn signal_set(&self, signal: Signal) {
        self.signal_change(Signal::empty(), signal);
    }

    pub fn signal_clear(&self, signal: Signal) {
        self.signal_change(signal, Signal::empty());
    }

    /// Add `callback` for signal status changes.
    ///
    /// The `callback` is a function of `Fn(Signal) -> bool`.
    /// It returns a bool indicating whether the handle process is over.
    /// If true, the function will never be called again.
    pub fn add_signal_callback(&self, callback: SignalHandler) {
        let mut inner = self.inner.lock();
        inner.signal_callbacks.push(callback);
    }

    /// Block until at least one `signal` assert. Return the current signal.
    pub fn wait_signal(&self, signal: Signal) -> Signal {
        let mut current_signal = self.signal();
        if !(current_signal & signal).is_empty() {
            return current_signal;
        }
        let waker = crate::hal::Thread::get_waker();
        self.add_signal_callback(Box::new(move |s| {
            if !(s & signal).is_empty() {
                waker.wake();
                return true;
            }
            false
        }));
        while (current_signal & signal).is_empty() {
            crate::hal::Thread::park();
            current_signal = self.signal();
        }
        current_signal
    }
}

impl dyn KernelObject {
    /// Asynchronous wait for one of `signal`.
    pub fn wait_signal_async(self: &Arc<Self>, signal: Signal) -> impl Future<Output = Signal> {
        struct SignalFuture {
            object: Arc<dyn KernelObject>,
            signal: Signal,
            first: bool,
        }

        impl Future for SignalFuture {
            type Output = Signal;

            fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
                let current_signal = self.object.signal();
                if !(current_signal & self.signal).is_empty() {
                    return Poll::Ready(current_signal);
                }
                if self.first {
                    self.object.add_signal_callback(Box::new({
                        let signal = self.signal;
                        let waker = cx.waker().clone();
                        move |s| {
                            if !(s & signal).is_empty() {
                                waker.wake_by_ref();
                                return true;
                            }
                            false
                        }
                    }));
                    self.first = false;
                }
                Poll::Pending
            }
        }

        SignalFuture {
            object: self.clone(),
            signal,
            first: true,
        }
    }
}

#[macro_export]
macro_rules! impl_kobject {
    ($class:ident) => {
        impl crate::object::KernelObject for $class {
            fn id(&self) -> KoID {
                self.base.id
            }
            fn type_name(&self) -> &'static str {
                stringify!($class)
            }
            fn signal(&self) -> Signal {
                self.base.signal()
            }
            fn add_signal_callback(&self, callback: SignalHandler) {
                self.base.add_signal_callback(callback);
            }
        }
        impl core::fmt::Debug for $class {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> Result<(), core::fmt::Error> {
                f.debug_tuple("KObject")
                    .field(&self.id())
                    .field(&self.type_name())
                    .finish()
            }
        }
    };
}

pub type KoID = u64;

pub type SignalHandler = Box<dyn Fn(Signal) -> bool + Send>;

#[cfg(test)]
mod tests {
    use super::*;
    use futures::task::SpawnExt;
    use std::time::Duration;

    struct DummyObject {
        base: KObjectBase,
    }

    impl_kobject!(DummyObject);

    impl DummyObject {
        fn new() -> Arc<Self> {
            Arc::new(DummyObject {
                base: KObjectBase::new(),
            })
        }
    }

    #[test]
    fn wait() {
        let object = DummyObject::new();
        let flag = Arc::new(AtomicU8::new(0));
        std::thread::spawn({
            let object = object.clone();
            let flag = flag.clone();
            move || {
                flag.store(1, Ordering::SeqCst);
                object.base.signal_set(Signal::READABLE);
                std::thread::sleep(Duration::from_millis(1));

                flag.store(2, Ordering::SeqCst);
                object.base.signal_set(Signal::WRITABLE);
            }
        });
        assert_eq!(flag.load(Ordering::SeqCst), 0);

        let signal = object.base.wait_signal(Signal::READABLE);
        assert_eq!(signal, Signal::READABLE);
        assert_eq!(flag.load(Ordering::SeqCst), 1);

        let signal = object.base.wait_signal(Signal::WRITABLE);
        assert_eq!(signal, Signal::READABLE | Signal::WRITABLE);
        assert_eq!(flag.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn wait_async() {
        let object = DummyObject::new();
        let flag = Arc::new(AtomicU8::new(0));

        let mut pool = futures::executor::LocalPool::new();
        pool.spawner()
            .spawn({
                let object = object.clone();
                let flag = flag.clone();
                async move {
                    flag.store(1, Ordering::SeqCst);
                    object.base.signal_set(Signal::READABLE);
                }
            })
            .unwrap();
        let object: Arc<dyn KernelObject> = object;
        assert_eq!(flag.load(Ordering::SeqCst), 0);

        let signal = pool.run_until(object.wait_signal_async(Signal::READABLE));
        assert_eq!(signal, Signal::READABLE);
        assert_eq!(flag.load(Ordering::SeqCst), 1);
    }
}
