// Copyright 2018-2020 the Deno authors. All rights reserved. MIT license.

// Do not add any dependency to modules.rs!
// modules.rs is complex and should remain decoupled from isolate.rs to keep the
// Isolate struct from becoming too bloating for users who do not need
// asynchronous module loading.

use rusty_v8 as v8;

use crate::any_error::ErrBox;
use crate::bindings;
use crate::js_errors::CoreJSError;
use crate::js_errors::V8Exception;
use crate::ops::*;
use crate::shared_queue::SharedQueue;
use crate::shared_queue::RECOMMENDED_SIZE;
use futures::future::FutureExt;
use futures::future::TryFutureExt;
use futures::stream::select;
use futures::stream::FuturesUnordered;
use futures::stream::StreamExt;
use futures::task::AtomicWaker;
use futures::Future;
use libc::c_void;
use std::cell::RefCell;
use std::collections::HashMap;
use std::convert::From;
use std::error::Error;
use std::fmt;
use std::ops::{Deref, DerefMut};
use std::option::Option;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{Arc, Once};
use std::task::Context;
use std::task::Poll;

/// A ZeroCopyBuf encapsulates a slice that's been borrowed from a JavaScript
/// ArrayBuffer object. JavaScript objects can normally be garbage collected,
/// but the existence of a ZeroCopyBuf inhibits this until it is dropped. It
/// behaves much like an Arc<[u8]>, although a ZeroCopyBuf currently can't be
/// cloned.
pub struct ZeroCopyBuf {
  backing_store: v8::SharedRef<v8::BackingStore>,
  byte_offset: usize,
  byte_length: usize,
}

unsafe impl Send for ZeroCopyBuf {}

impl ZeroCopyBuf {
  pub fn new(view: v8::Local<v8::ArrayBufferView>) -> Self {
    let backing_store = view.buffer().unwrap().get_backing_store();
    let byte_offset = view.byte_offset();
    let byte_length = view.byte_length();
    Self {
      backing_store,
      byte_offset,
      byte_length,
    }
  }
}

impl Deref for ZeroCopyBuf {
  type Target = [u8];
  fn deref(&self) -> &[u8] {
    let buf = unsafe { &**self.backing_store.get() };
    &buf[self.byte_offset..self.byte_offset + self.byte_length]
  }
}

impl DerefMut for ZeroCopyBuf {
  fn deref_mut(&mut self) -> &mut [u8] {
    let buf = unsafe { &mut **self.backing_store.get() };
    &mut buf[self.byte_offset..self.byte_offset + self.byte_length]
  }
}

impl AsRef<[u8]> for ZeroCopyBuf {
  fn as_ref(&self) -> &[u8] {
    &*self
  }
}

impl AsMut<[u8]> for ZeroCopyBuf {
  fn as_mut(&mut self) -> &mut [u8] {
    &mut *self
  }
}

pub enum SnapshotConfig {
  Borrowed(v8::StartupData<'static>),
  Owned(v8::OwnedStartupData),
}

impl From<&'static [u8]> for SnapshotConfig {
  fn from(sd: &'static [u8]) -> Self {
    Self::Borrowed(v8::StartupData::new(sd))
  }
}

impl From<v8::OwnedStartupData> for SnapshotConfig {
  fn from(sd: v8::OwnedStartupData) -> Self {
    Self::Owned(sd)
  }
}

impl Deref for SnapshotConfig {
  type Target = v8::StartupData<'static>;
  fn deref(&self) -> &Self::Target {
    match self {
      Self::Borrowed(sd) => sd,
      Self::Owned(sd) => &*sd,
    }
  }
}

/// Stores a script used to initalize a Isolate
pub struct Script<'a> {
  pub source: &'a str,
  pub filename: &'a str,
}

// TODO(ry) It's ugly that we have both Script and OwnedScript. Ideally we
// wouldn't expose such twiddly complexity.
struct OwnedScript {
  pub source: String,
  pub filename: String,
}

impl From<Script<'_>> for OwnedScript {
  fn from(s: Script) -> OwnedScript {
    OwnedScript {
      source: s.source.to_string(),
      filename: s.filename.to_string(),
    }
  }
}

/// Represents data used to initialize isolate at startup
/// either a binary snapshot or a javascript source file
/// in the form of the StartupScript struct.
pub enum StartupData<'a> {
  Script(Script<'a>),
  Snapshot(&'static [u8]),
  OwnedSnapshot(v8::OwnedStartupData),
  None,
}

type JSErrorCreateFn = dyn Fn(V8Exception) -> ErrBox;
type IsolateErrorHandleFn = dyn FnMut(ErrBox) -> Result<(), ErrBox>;

/// A single execution context of JavaScript. Corresponds roughly to the "Web
/// Worker" concept in the DOM. An Isolate is a Future that can be used with
/// Tokio.  The Isolate future complete when there is an error or when all
/// pending ops have completed.
///
/// Ops are created in JavaScript by calling Deno.core.dispatch(), and in Rust
/// by implementing dispatcher function that takes control buffer and optional zero copy buffer
/// as arguments. An async Op corresponds exactly to a Promise in JavaScript.
#[allow(unused)]
pub struct IsolateInner {
  pub(crate) v8_isolate: Option<v8::OwnedIsolate>,
  snapshot_creator: Option<v8::SnapshotCreator>,
  has_snapshotted: bool,
  snapshot: Option<SnapshotConfig>,
  pub(crate) last_exception: Option<String>,
  pub(crate) global_context: v8::Global<v8::Context>,
  pub(crate) shared_ab: v8::Global<v8::SharedArrayBuffer>,
  pub(crate) js_recv_cb: v8::Global<v8::Function>,
  pub(crate) pending_promise_exceptions: HashMap<i32, v8::Global<v8::Value>>,
  js_error_create: Arc<JSErrorCreateFn>,
  needs_init: bool,
  pub(crate) shared: SharedQueue,
  pending_ops: FuturesUnordered<PendingOpFuture>,
  pending_unref_ops: FuturesUnordered<PendingOpFuture>,
  have_unpolled_ops: bool,
  startup_script: Option<OwnedScript>,
  pub op_registry: Rc<OpRegistry>,
  waker: AtomicWaker,
  error_handler: Option<Box<IsolateErrorHandleFn>>,
  magic_number: usize,
}

pub struct Isolate(pub Rc<RefCell<IsolateInner>>);

impl Drop for Isolate {
  fn drop(&mut self) {
    let isolate = self.0.borrow_mut().v8_isolate.take().unwrap();
    if let Some(creator) = self.0.borrow_mut().snapshot_creator.take() {
      // TODO(ry) V8 has a strange assert which prevents a SnapshotCreator from
      // being deallocated if it hasn't created a snapshot yet.
      // https://github.com/v8/v8/blob/73212783fbd534fac76cc4b66aac899c13f71fc8/src/api.cc#L603
      // If that assert is removed, this if guard could be removed.
      // WARNING: There may be false positive LSAN errors here.
      std::mem::forget(isolate);
      if self.0.borrow().has_snapshotted {
        drop(creator);
      }
    } else {
      drop(isolate);
    }
  }
}

static DENO_INIT: Once = Once::new();

#[allow(clippy::missing_safety_doc)]
pub unsafe fn v8_init() {
  let platform = v8::new_default_platform();
  v8::V8::initialize_platform(platform);
  v8::V8::initialize();
  // TODO(ry) This makes WASM compile synchronously. Eventually we should
  // remove this to make it work asynchronously too. But that requires getting
  // PumpMessageLoop and RunMicrotasks setup correctly.
  // See https://github.com/denoland/deno/issues/2544
  let argv = vec![
    "".to_string(),
    "--no-wasm-async-compilation".to_string(),
    "--harmony-top-level-await".to_string(),
  ];
  v8::V8::set_flags_from_command_line(argv);
}

impl Isolate {
  pub fn from_v8(isolate: &v8::Isolate) -> Isolate {
    let ptr = isolate.get_data(0) as *const RefCell<IsolateInner>;
    let isolate_rc = unsafe { Rc::from_raw(ptr) };
    let ptr2 = Rc::into_raw(Rc::clone(&isolate_rc));
    assert_eq!(ptr, ptr2);
    assert_eq!(isolate_rc.borrow().magic_number, 0xCAFE_BABE);
    Isolate(isolate_rc)
  }

  /// startup_data defines the snapshot or script used at startup to initialize
  /// the isolate.
  pub fn new(startup_data: StartupData, will_snapshot: bool) -> Self {
    DENO_INIT.call_once(|| {
      unsafe { v8_init() };
    });

    let mut load_snapshot: Option<SnapshotConfig> = None;
    let mut startup_script: Option<OwnedScript> = None;

    // Separate into Option values for each startup type
    match startup_data {
      StartupData::Script(d) => {
        startup_script = Some(d.into());
      }
      StartupData::Snapshot(d) => {
        load_snapshot = Some(d.into());
      }
      StartupData::OwnedSnapshot(d) => {
        load_snapshot = Some(d.into());
      }
      StartupData::None => {}
    };

    let mut global_context = v8::Global::<v8::Context>::new();
    let (mut isolate, maybe_snapshot_creator) = if will_snapshot {
      // TODO(ry) Support loading snapshots before snapshotting.
      assert!(load_snapshot.is_none());
      let mut creator =
        v8::SnapshotCreator::new(Some(&bindings::EXTERNAL_REFERENCES));
      let isolate = unsafe { creator.get_owned_isolate() };
      let isolate = Isolate::setup_isolate(isolate);

      let mut hs = v8::HandleScope::new2(&isolate);
      let scope = hs.enter();

      let context = bindings::initialize_context(scope);
      global_context.set(scope, context);
      creator.set_default_context(context);

      (isolate, Some(creator))
    } else {
      let mut params = v8::Isolate::create_params();
      params.set_array_buffer_allocator(v8::new_default_allocator());
      params.set_external_references(&bindings::EXTERNAL_REFERENCES);
      if let Some(ref mut snapshot) = load_snapshot {
        params.set_snapshot_blob(snapshot);
      }

      let isolate = v8::Isolate::new(params);
      let isolate = Isolate::setup_isolate(isolate);

      let mut hs = v8::HandleScope::new2(&isolate);
      let scope = hs.enter();

      let context = match load_snapshot {
        Some(_) => v8::Context::new(scope),
        None => {
          // If no snapshot is provided, we initialize the context with empty
          // main source code and source maps.
          bindings::initialize_context(scope)
        }
      };
      global_context.set(scope, context);

      (isolate, None)
    };

    let shared = SharedQueue::new(RECOMMENDED_SIZE);
    let needs_init = true;

    let inner = IsolateInner {
      v8_isolate: None,
      last_exception: None,
      global_context,
      pending_promise_exceptions: HashMap::new(),
      shared_ab: v8::Global::<v8::SharedArrayBuffer>::new(),
      js_recv_cb: v8::Global::<v8::Function>::new(),
      snapshot_creator: maybe_snapshot_creator,
      snapshot: load_snapshot,
      has_snapshotted: false,
      js_error_create: Arc::new(CoreJSError::from_v8_exception),
      shared,
      needs_init,
      pending_ops: FuturesUnordered::new(),
      pending_unref_ops: FuturesUnordered::new(),
      have_unpolled_ops: false,
      startup_script,
      op_registry: Rc::new(OpRegistry::new()),
      waker: AtomicWaker::new(),
      error_handler: None,
      magic_number: 0xCAFE_BABE,
    };

    let isolate_rc = Rc::new(RefCell::new(inner));
    {
      let ptr = Rc::into_raw(Rc::clone(&isolate_rc));
      assert_eq!(isolate.get_data(0), std::ptr::null_mut());
      unsafe { isolate.set_data(0, ptr as *mut c_void) };
      isolate_rc.borrow_mut().v8_isolate = Some(isolate);
    }

    Isolate(isolate_rc)
  }

  // This is a terrible hack.
  pub fn v8_isolate(&self) -> &v8::Isolate {
    let inner = self.0.borrow();
    let ref_owned = inner.v8_isolate.as_ref().unwrap();
    let ref_isolate: &v8::Isolate = &ref_owned;
    let ptr: *const v8::Isolate = ref_isolate;
    unsafe { &*ptr }
  }

  pub fn global_context<'a>(
    &self,
    scope: &mut impl v8::ToLocal<'a>,
  ) -> v8::Local<'a, v8::Context> {
    let mut hs = v8::EscapableHandleScope::new(scope);
    let scope = hs.enter();
    let c = self.0.borrow().global_context.get(scope).unwrap();
    scope.escape(c)
  }

  pub fn setup_isolate(mut isolate: v8::OwnedIsolate) -> v8::OwnedIsolate {
    isolate.set_capture_stack_trace_for_uncaught_exceptions(true, 10);
    isolate.set_promise_reject_callback(bindings::promise_reject_callback);
    isolate.add_message_listener(bindings::message_callback);
    isolate
  }

  pub fn exception_to_err_result<'a, T>(
    &mut self,
    scope: &mut impl v8::ToLocal<'a>,
    exception: v8::Local<v8::Value>,
  ) -> Result<T, ErrBox> {
    self.handle_exception(scope, exception);
    self.check_last_exception().map(|_| unreachable!())
  }

  pub fn handle_exception<'a>(
    &mut self,
    scope: &mut impl v8::ToLocal<'a>,
    exception: v8::Local<v8::Value>,
  ) {
    // Use a HandleScope because the  functions below create a lot of
    // local handles (in particular, `encode_message_as_json()` does).
    let mut hs = v8::HandleScope::new(scope);
    let scope = hs.enter();

    let handle = scope.isolate().thread_safe_handle();

    let is_terminating_exception = handle.is_execution_terminating();
    let mut exception = exception;

    if is_terminating_exception {
      // TerminateExecution was called. Cancel exception termination so that the
      // exception can be created..
      handle.cancel_terminate_execution();

      // Maybe make a new exception object.
      if exception.is_null_or_undefined() {
        let exception_str =
          v8::String::new(scope, "execution terminated").unwrap();
        exception = v8::Exception::error(scope, exception_str);
      }
    }

    let message = v8::Exception::create_message(scope, exception);
    let json_str = self.encode_message_as_json(scope, message);
    self.0.borrow_mut().last_exception = Some(json_str);

    if is_terminating_exception {
      // Re-enable exception termination.
      handle.terminate_execution();
    }
  }

  pub fn encode_message_as_json<'a>(
    &mut self,
    scope: &mut impl v8::ToLocal<'a>,
    message: v8::Local<v8::Message>,
  ) -> String {
    let context = scope.get_current_context().unwrap();
    let json_obj = bindings::encode_message_as_object(scope, message);
    let json_string = v8::json::stringify(context, json_obj.into()).unwrap();
    json_string.to_rust_string_lossy(scope)
  }

  /// Defines the how Deno.core.dispatch() acts.
  /// Called whenever Deno.core.dispatch() is called in JavaScript. zero_copy_buf
  /// corresponds to the second argument of Deno.core.dispatch().
  ///
  /// Requires runtime to explicitly ask for op ids before using any of the ops.
  pub fn register_op<F>(&self, name: &str, op: F) -> OpId
  where
    F: Fn(&[u8], Option<ZeroCopyBuf>) -> CoreOp + 'static,
  {
    self.0.borrow().op_registry.register(name, op)
  }

  /// Allows a callback to be set whenever a V8 exception is made. This allows
  /// the caller to wrap the V8Exception into an error. By default this callback
  /// is set to CoreJSError::from_v8_exception.
  pub fn set_js_error_create<F>(&mut self, f: F)
  where
    F: Fn(V8Exception) -> ErrBox + 'static,
  {
    let mut inner = self.0.borrow_mut();
    inner.js_error_create = Arc::new(f);
  }

  /// Get a thread safe handle on the isolate.
  pub fn shared_isolate_handle(&mut self) -> v8::IsolateHandle {
    let mut inner = self.0.borrow_mut();
    inner.v8_isolate.as_mut().unwrap().thread_safe_handle()
  }

  /// Executes a bit of built-in JavaScript to provide Deno.sharedQueue.
  pub(crate) fn shared_init(&mut self) {
    if self.0.borrow().needs_init {
      self.0.borrow_mut().needs_init = false;
      js_check(
        self.execute("shared_queue.js", include_str!("shared_queue.js")),
      );
      // Maybe execute the startup script.
      let mut inner = self.0.borrow_mut();
      if let Some(s) = inner.startup_script.take() {
        drop(inner);
        self.execute(&s.filename, &s.source).unwrap()
      }
    }
  }

  pub fn dispatch_op<'s>(
    &mut self,
    scope: &mut impl v8::ToLocal<'s>,
    op_id: OpId,
    control_buf: &[u8],
    zero_copy_buf: Option<ZeroCopyBuf>,
  ) -> Option<(OpId, Box<[u8]>)> {
    let maybe_op =
      self
        .0
        .borrow_mut()
        .op_registry
        .call(op_id, control_buf, zero_copy_buf);

    let op = match maybe_op {
      Some(op) => op,
      None => {
        let message =
          v8::String::new(scope, &format!("Unknown op id: {}", op_id)).unwrap();
        let exception = v8::Exception::type_error(scope, message);
        scope.isolate().throw_exception(exception);
        return None;
      }
    };

    debug_assert_eq!(self.0.borrow().shared.size(), 0);
    match op {
      Op::Sync(buf) => {
        // For sync messages, we always return the response via Deno.core.send's
        // return value. Sync messages ignore the op_id.
        let op_id = 0;
        Some((op_id, buf))
      }
      Op::Async(fut) => {
        let fut2 = fut.map_ok(move |buf| (op_id, buf));
        self.0.borrow_mut().pending_ops.push(fut2.boxed_local());
        self.0.borrow_mut().have_unpolled_ops = true;
        None
      }
      Op::AsyncUnref(fut) => {
        let fut2 = fut.map_ok(move |buf| (op_id, buf));
        self
          .0
          .borrow_mut()
          .pending_unref_ops
          .push(fut2.boxed_local());
        self.0.borrow_mut().have_unpolled_ops = true;
        None
      }
    }
  }

  /// Executes traditional JavaScript code (traditional = not ES modules)
  ///
  /// ErrBox can be downcast to a type that exposes additional information about
  /// the V8 exception. By default this type is CoreJSError, however it may be a
  /// different type if Isolate::set_js_error_create() has been used.
  pub fn execute(
    &mut self,
    js_filename: &str,
    js_source: &str,
  ) -> Result<(), ErrBox> {
    self.shared_init();

    let mut hs =
      v8::HandleScope::new2(self.0.borrow_mut().v8_isolate.as_mut().unwrap());
    let scope = hs.enter();
    let context = self.global_context(scope);
    let mut cs = v8::ContextScope::new(scope, context);
    let scope = cs.enter();

    let source = v8::String::new(scope, js_source).unwrap();
    let name = v8::String::new(scope, js_filename).unwrap();
    let origin = bindings::script_origin(scope, name);

    let mut try_catch = v8::TryCatch::new(scope);
    let tc = try_catch.enter();

    let mut script =
      v8::Script::compile(scope, context, source, Some(&origin)).unwrap();
    match script.run(scope, context) {
      Some(_) => Ok(()),
      None => {
        assert!(tc.has_caught());
        let exception = tc.exception().unwrap();
        self.exception_to_err_result(scope, exception)
      }
    }
  }

  pub(crate) fn check_last_exception(&mut self) -> Result<(), ErrBox> {
    let mut inner = self.0.borrow_mut();
    match inner.last_exception.take() {
      None => Ok(()),
      Some(json_str) => {
        let v8_exception = V8Exception::from_json(&json_str).unwrap();
        drop(inner);
        let js_error = (self.0.borrow().js_error_create)(v8_exception);
        Err(js_error)
      }
    }
  }

  pub(crate) fn attach_handle_to_error(
    &mut self,
    scope: &mut impl v8::InIsolate,
    err: ErrBox,
    handle: v8::Local<v8::Value>,
  ) -> ErrBox {
    ErrWithV8Handle::new(scope, err, handle).into()
  }

  fn check_promise_exceptions<'s>(
    &mut self,
    scope: &mut impl v8::ToLocal<'s>,
  ) -> Result<(), ErrBox> {
    let mut inner = self.0.borrow_mut();
    if let Some(&key) = inner.pending_promise_exceptions.keys().next() {
      let mut handle = inner.pending_promise_exceptions.remove(&key).unwrap();
      let exception = handle.get(scope).expect("empty error handle");
      handle.reset(scope);
      drop(inner);
      self.exception_to_err_result(scope, exception)
    } else {
      Ok(())
    }
  }

  fn async_op_response<'s>(
    &mut self,
    scope: &mut impl v8::ToLocal<'s>,
    maybe_buf: Option<(OpId, Box<[u8]>)>,
  ) -> Result<(), ErrBox> {
    let context = scope.get_current_context().unwrap();
    let global: v8::Local<v8::Value> = context.global(scope).into();
    let js_recv_cb = self
      .0
      .borrow()
      .js_recv_cb
      .get(scope)
      .expect("Deno.core.recv has not been called.");

    // TODO(piscisaureus): properly integrate TryCatch in the scope chain.
    let mut try_catch = v8::TryCatch::new(scope);
    let tc = try_catch.enter();

    match maybe_buf {
      Some((op_id, buf)) => {
        let op_id: v8::Local<v8::Value> =
          v8::Integer::new(scope, op_id as i32).into();
        let ui8: v8::Local<v8::Value> =
          bindings::boxed_slice_to_uint8array(scope, buf).into();
        js_recv_cb.call(scope, context, global, &[op_id, ui8])
      }
      None => js_recv_cb.call(scope, context, global, &[]),
    };

    match tc.exception() {
      None => Ok(()),
      Some(exception) => self.exception_to_err_result(scope, exception),
    }
  }

  /// Takes a snapshot. The isolate should have been created with will_snapshot
  /// set to true.
  ///
  /// ErrBox can be downcast to a type that exposes additional information about
  /// the V8 exception. By default this type is CoreJSError, however it may be a
  /// different type if Isolate::set_js_error_create() has been used.
  pub fn snapshot(&mut self) -> Result<v8::OwnedStartupData, ErrBox> {
    assert!(self.0.borrow().snapshot_creator.is_some());

    let mut hs = v8::HandleScope::new2(self.v8_isolate());
    let scope = hs.enter();
    self.0.borrow_mut().global_context.reset(scope);

    let mut snapshot_creator =
      self.0.borrow_mut().snapshot_creator.take().unwrap();
    let snapshot = snapshot_creator
      .create_blob(v8::FunctionCodeHandling::Keep)
      .unwrap();
    self.0.borrow_mut().has_snapshotted = true;
    self.0.borrow_mut().snapshot_creator = Some(snapshot_creator);

    self.check_last_exception().map(|_| snapshot)
  }
}

impl Future for Isolate {
  type Output = Result<(), ErrBox>;

  fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
    let isolate = &mut self;
    //let isolate = self.get_mut();
    isolate.0.borrow().waker.register(cx.waker());
    isolate.shared_init();

    let mut hs = v8::HandleScope::new2(isolate.v8_isolate());
    let scope = hs.enter();
    let context = isolate.global_context(scope);
    let mut cs = v8::ContextScope::new(scope, context);
    let scope = cs.enter();

    isolate.check_promise_exceptions(scope)?;

    let mut overflow_response: Option<(OpId, Buf)> = None;

    loop {
      // Now handle actual ops.
      isolate.0.borrow_mut().have_unpolled_ops = false;
      /*
      match select(
        &mut isolate.0.borrow_mut().pending_ops,
        &mut isolate.0.borrow_mut().pending_unref_ops,
      )
      */
      #[allow(clippy::match_wild_err_arm)]
      match isolate.0.borrow_mut().pending_ops.poll_next_unpin(cx) {
        Poll::Ready(Some(Err(_))) => panic!("unexpected op error"),
        Poll::Ready(None) => break,
        Poll::Pending => break,
        Poll::Ready(Some(Ok((op_id, buf)))) => {
          let successful_push = isolate.0.borrow_mut().shared.push(op_id, &buf);
          if !successful_push {
            // If we couldn't push the response to the shared queue, because
            // there wasn't enough size, we will return the buffer via the
            // legacy route, using the argument of deno_respond.
            overflow_response = Some((op_id, buf));
            break;
          }
        }
      }
    }

    if isolate.0.borrow().shared.size() > 0 {
      isolate.async_op_response(scope, None)?;
      // The other side should have shifted off all the messages.
      assert_eq!(isolate.0.borrow().shared.size(), 0);
    }

    if overflow_response.is_some() {
      let (op_id, buf) = overflow_response.take().unwrap();
      isolate.async_op_response(scope, Some((op_id, buf)))?;
    }

    isolate.check_promise_exceptions(scope)?;

    // We're idle if pending_ops is empty.
    if isolate.0.borrow().pending_ops.is_empty() {
      Poll::Ready(Ok(()))
    } else {
      if isolate.0.borrow().have_unpolled_ops {
        isolate.0.borrow().waker.wake();
      }
      Poll::Pending
    }
  }
}

pub fn js_check<T>(r: Result<T, ErrBox>) -> T {
  if let Err(e) = r {
    panic!(e.to_string());
  }
  r.unwrap()
}

#[cfg(test)]
pub mod tests {
  use super::*;
  use futures::future::lazy;
  use std::ops::FnOnce;
  use std::sync::atomic::{AtomicUsize, Ordering};

  pub fn run_in_task<F>(f: F)
  where
    F: FnOnce(&mut Context) + Send + 'static,
  {
    futures::executor::block_on(lazy(move |cx| f(cx)));
  }

  fn poll_until_ready<F>(future: &mut F, max_poll_count: usize) -> F::Output
  where
    F: Future + Unpin,
  {
    let mut cx = Context::from_waker(futures::task::noop_waker_ref());
    for _ in 0..max_poll_count {
      match future.poll_unpin(&mut cx) {
        Poll::Pending => continue,
        Poll::Ready(val) => return val,
      }
    }
    panic!(
      "Isolate still not ready after polling {} times.",
      max_poll_count
    )
  }

  pub enum Mode {
    Async,
    AsyncUnref,
    OverflowReqSync,
    OverflowResSync,
    OverflowReqAsync,
    OverflowResAsync,
  }

  pub fn setup(mode: Mode) -> (Isolate, Arc<AtomicUsize>) {
    let dispatch_count = Arc::new(AtomicUsize::new(0));
    let dispatch_count_ = dispatch_count.clone();

    let mut isolate = Isolate::new(StartupData::None, false);

    let dispatcher =
      move |control: &[u8], _zero_copy: Option<ZeroCopyBuf>| -> CoreOp {
        dispatch_count_.fetch_add(1, Ordering::Relaxed);
        match mode {
          Mode::Async => {
            assert_eq!(control.len(), 1);
            assert_eq!(control[0], 42);
            let buf = vec![43u8].into_boxed_slice();
            Op::Async(futures::future::ok(buf).boxed())
          }
          Mode::AsyncUnref => {
            assert_eq!(control.len(), 1);
            assert_eq!(control[0], 42);
            let fut = async {
              // This future never finish.
              futures::future::pending::<()>().await;
              let buf = vec![43u8].into_boxed_slice();
              Ok(buf)
            };
            Op::AsyncUnref(fut.boxed())
          }
          Mode::OverflowReqSync => {
            assert_eq!(control.len(), 100 * 1024 * 1024);
            let buf = vec![43u8].into_boxed_slice();
            Op::Sync(buf)
          }
          Mode::OverflowResSync => {
            assert_eq!(control.len(), 1);
            assert_eq!(control[0], 42);
            let mut vec = Vec::<u8>::new();
            vec.resize(100 * 1024 * 1024, 0);
            vec[0] = 99;
            let buf = vec.into_boxed_slice();
            Op::Sync(buf)
          }
          Mode::OverflowReqAsync => {
            assert_eq!(control.len(), 100 * 1024 * 1024);
            let buf = vec![43u8].into_boxed_slice();
            Op::Async(futures::future::ok(buf).boxed())
          }
          Mode::OverflowResAsync => {
            assert_eq!(control.len(), 1);
            assert_eq!(control[0], 42);
            let mut vec = Vec::<u8>::new();
            vec.resize(100 * 1024 * 1024, 0);
            vec[0] = 4;
            let buf = vec.into_boxed_slice();
            Op::Async(futures::future::ok(buf).boxed())
          }
        }
      };

    isolate.register_op("test", dispatcher);

    js_check(isolate.execute(
      "setup.js",
      r#"
        function assert(cond) {
          if (!cond) {
            throw Error("assert");
          }
        }
        "#,
    ));
    assert_eq!(dispatch_count.load(Ordering::Relaxed), 0);
    (isolate, dispatch_count)
  }

  #[test]
  fn test_dispatch() {
    let (mut isolate, dispatch_count) = setup(Mode::Async);
    js_check(isolate.execute(
      "filename.js",
      r#"
        let control = new Uint8Array([42]);
        Deno.core.print("Deno.core.print hello\n");
        Deno.core.send(1, control);
        async function main() {
          //Deno.core.send(1, control);
        }
        main();
        "#,
    ));
    assert_eq!(dispatch_count.load(Ordering::Relaxed), 2);
  }

  #[test]
  fn test_poll_async_delayed_ops() {
    run_in_task(|cx| {
      let (mut isolate, dispatch_count) = setup(Mode::Async);

      js_check(isolate.execute(
        "setup2.js",
        r#"
         let nrecv = 0;
         Deno.core.setAsyncHandler(1, (buf) => {
           nrecv++;
         });
         "#,
      ));
      assert_eq!(dispatch_count.load(Ordering::Relaxed), 0);
      js_check(isolate.execute(
        "check1.js",
        r#"
         assert(nrecv == 0);
         let control = new Uint8Array([42]);
         Deno.core.send(1, control);
         assert(nrecv == 0);
         "#,
      ));
      assert_eq!(dispatch_count.load(Ordering::Relaxed), 1);
      assert!(match isolate.poll_unpin(cx) {
        Poll::Ready(Ok(_)) => true,
        _ => false,
      });
      assert_eq!(dispatch_count.load(Ordering::Relaxed), 1);
      js_check(isolate.execute(
        "check2.js",
        r#"
         assert(nrecv == 1);
         Deno.core.send(1, control);
         assert(nrecv == 1);
         "#,
      ));
      assert_eq!(dispatch_count.load(Ordering::Relaxed), 2);
      assert!(match isolate.poll_unpin(cx) {
        Poll::Ready(Ok(_)) => true,
        _ => false,
      });
      js_check(isolate.execute("check3.js", "assert(nrecv == 2)"));
      assert_eq!(dispatch_count.load(Ordering::Relaxed), 2);
      // We are idle, so the next poll should be the last.
      assert!(match isolate.poll_unpin(cx) {
        Poll::Ready(Ok(_)) => true,
        _ => false,
      });
    });
  }

  #[test]
  fn test_poll_async_optional_ops() {
    run_in_task(|cx| {
      let (mut isolate, dispatch_count) = setup(Mode::AsyncUnref);
      js_check(isolate.execute(
        "check1.js",
        r#"
          Deno.core.setAsyncHandler(1, (buf) => {
            // This handler will never be called
            assert(false);
          });
          let control = new Uint8Array([42]);
          Deno.core.send(1, control);
        "#,
      ));
      assert_eq!(dispatch_count.load(Ordering::Relaxed), 1);
      // The above op never finish, but isolate can finish
      // because the op is an unreffed async op.
      assert!(match isolate.poll_unpin(cx) {
        Poll::Ready(Ok(_)) => true,
        _ => false,
      });
    })
  }

  #[test]
  fn dangling_shared_isolate() {
    let shared = {
      // isolate is dropped at the end of this block
      let (mut isolate, _dispatch_count) = setup(Mode::Async);
      isolate.shared_isolate_handle()
    };

    // this should not SEGFAULT
    shared.terminate_execution();
  }

  #[test]
  fn overflow_req_sync() {
    let (mut isolate, dispatch_count) = setup(Mode::OverflowReqSync);
    js_check(isolate.execute(
      "overflow_req_sync.js",
      r#"
        let asyncRecv = 0;
        Deno.core.setAsyncHandler(1, (buf) => { asyncRecv++ });
        // Large message that will overflow the shared space.
        let control = new Uint8Array(100 * 1024 * 1024);
        let response = Deno.core.dispatch(1, control);
        assert(response instanceof Uint8Array);
        assert(response.length == 1);
        assert(response[0] == 43);
        assert(asyncRecv == 0);
        "#,
    ));
    assert_eq!(dispatch_count.load(Ordering::Relaxed), 1);
  }

  #[test]
  fn overflow_res_sync() {
    // TODO(ry) This test is quite slow due to memcpy-ing 100MB into JS. We
    // should optimize this.
    let (mut isolate, dispatch_count) = setup(Mode::OverflowResSync);
    js_check(isolate.execute(
      "overflow_res_sync.js",
      r#"
        let asyncRecv = 0;
        Deno.core.setAsyncHandler(1, (buf) => { asyncRecv++ });
        // Large message that will overflow the shared space.
        let control = new Uint8Array([42]);
        let response = Deno.core.dispatch(1, control);
        assert(response instanceof Uint8Array);
        assert(response.length == 100 * 1024 * 1024);
        assert(response[0] == 99);
        assert(asyncRecv == 0);
        "#,
    ));
    assert_eq!(dispatch_count.load(Ordering::Relaxed), 1);
  }

  #[test]
  fn overflow_req_async() {
    run_in_task(|cx| {
      let (mut isolate, dispatch_count) = setup(Mode::OverflowReqAsync);
      js_check(isolate.execute(
        "overflow_req_async.js",
        r#"
         let asyncRecv = 0;
         Deno.core.setAsyncHandler(1, (buf) => {
           assert(buf.byteLength === 1);
           assert(buf[0] === 43);
           asyncRecv++;
         });
         // Large message that will overflow the shared space.
         let control = new Uint8Array(100 * 1024 * 1024);
         let response = Deno.core.dispatch(1, control);
         // Async messages always have null response.
         assert(response == null);
         assert(asyncRecv == 0);
         "#,
      ));
      assert_eq!(dispatch_count.load(Ordering::Relaxed), 1);
      assert!(match isolate.poll_unpin(cx) {
        Poll::Ready(Ok(_)) => true,
        _ => false,
      });
      js_check(isolate.execute("check.js", "assert(asyncRecv == 1);"));
    });
  }

  #[test]
  fn overflow_res_async() {
    run_in_task(|_cx| {
      // TODO(ry) This test is quite slow due to memcpy-ing 100MB into JS. We
      // should optimize this.
      let (mut isolate, dispatch_count) = setup(Mode::OverflowResAsync);
      js_check(isolate.execute(
        "overflow_res_async.js",
        r#"
         let asyncRecv = 0;
         Deno.core.setAsyncHandler(1, (buf) => {
           assert(buf.byteLength === 100 * 1024 * 1024);
           assert(buf[0] === 4);
           asyncRecv++;
         });
         // Large message that will overflow the shared space.
         let control = new Uint8Array([42]);
         let response = Deno.core.dispatch(1, control);
         assert(response == null);
         assert(asyncRecv == 0);
         "#,
      ));
      assert_eq!(dispatch_count.load(Ordering::Relaxed), 1);
      poll_until_ready(&mut isolate, 3).unwrap();
      js_check(isolate.execute("check.js", "assert(asyncRecv == 1);"));
    });
  }

  #[test]
  fn overflow_res_multiple_dispatch_async() {
    // TODO(ry) This test is quite slow due to memcpy-ing 100MB into JS. We
    // should optimize this.
    run_in_task(|_cx| {
      let (mut isolate, dispatch_count) = setup(Mode::OverflowResAsync);
      js_check(isolate.execute(
        "overflow_res_multiple_dispatch_async.js",
        r#"
         let asyncRecv = 0;
         Deno.core.setAsyncHandler(1, (buf) => {
           assert(buf.byteLength === 100 * 1024 * 1024);
           assert(buf[0] === 4);
           asyncRecv++;
         });
         // Large message that will overflow the shared space.
         let control = new Uint8Array([42]);
         let response = Deno.core.dispatch(1, control);
         assert(response == null);
         assert(asyncRecv == 0);
         // Dispatch another message to verify that pending ops
         // are done even if shared space overflows
         Deno.core.dispatch(1, control);
         "#,
      ));
      assert_eq!(dispatch_count.load(Ordering::Relaxed), 2);
      poll_until_ready(&mut isolate, 3).unwrap();
      js_check(isolate.execute("check.js", "assert(asyncRecv == 2);"));
    });
  }

  #[test]
  fn test_pre_dispatch() {
    run_in_task(|mut cx| {
      let (mut isolate, _dispatch_count) = setup(Mode::OverflowResAsync);
      js_check(isolate.execute(
        "bad_op_id.js",
        r#"
          let thrown;
          try {
            Deno.core.dispatch(100, []);
          } catch (e) {
            thrown = e;
          }
          assert(String(thrown) === "TypeError: Unknown op id: 100");
         "#,
      ));
      if let Poll::Ready(Err(_)) = isolate.poll_unpin(&mut cx) {
        unreachable!();
      }
    });
  }

  #[test]
  fn test_js() {
    run_in_task(|mut cx| {
      let (mut isolate, _dispatch_count) = setup(Mode::Async);
      js_check(
        isolate.execute(
          "shared_queue_test.js",
          include_str!("shared_queue_test.js"),
        ),
      );
      if let Poll::Ready(Err(_)) = isolate.poll_unpin(&mut cx) {
        unreachable!();
      }
    });
  }

  #[test]
  fn will_snapshot() {
    let snapshot = {
      let mut isolate = Isolate::new(StartupData::None, true);
      js_check(isolate.execute("a.js", "a = 1 + 2"));
      let s = isolate.snapshot().unwrap();
      drop(isolate);
      s
    };

    let startup_data = StartupData::OwnedSnapshot(snapshot);
    let mut isolate2 = Isolate::new(startup_data, false);
    js_check(isolate2.execute("check.js", "if (a != 3) throw Error('x')"));
  }
}

// TODO(piscisaureus): rusty_v8 should implement the Error trait on
// values of type v8::Global<T>.
pub struct ErrWithV8Handle {
  err: ErrBox,
  handle: v8::Global<v8::Value>,
}

impl ErrWithV8Handle {
  pub fn new(
    scope: &mut impl v8::InIsolate,
    err: ErrBox,
    handle: v8::Local<v8::Value>,
  ) -> Self {
    let handle = v8::Global::new_from(scope, handle);
    Self { err, handle }
  }

  pub fn get_handle(&mut self) -> &mut v8::Global<v8::Value> {
    &mut self.handle
  }
}

unsafe impl Send for ErrWithV8Handle {}
unsafe impl Sync for ErrWithV8Handle {}

impl Error for ErrWithV8Handle {}

impl fmt::Display for ErrWithV8Handle {
  fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
    self.err.fmt(f)
  }
}

impl fmt::Debug for ErrWithV8Handle {
  fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
    self.err.fmt(f)
  }
}
