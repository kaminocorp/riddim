//! Host-side COM objects handed to VST3 plugins.
//!
//! Two families live here:
//!
//! * Main-thread objects (`MemoryStream`, `HostApplication`, `HostMessage`, `HostAttributeList`,
//!   `ComponentHandler`, `PlugFrame`). They may allocate.
//! * Audio-thread objects (`EventList`, `ParamChanges`, `ParamValueQueue`). They are sized once
//!   at activation and never touch the heap afterwards.

use std::cell::{Cell, RefCell, UnsafeCell};
use std::collections::HashMap;
use std::ffi::{c_void, CStr};
use std::sync::Arc;

use crossbeam_queue::ArrayQueue;
use vst3::Steinberg::Vst::{
    IAttributeList, IAttributeListTrait, IAttributeList_::AttrID, IComponentHandler,
    IComponentHandler2, IComponentHandler2Trait, IComponentHandlerTrait, IEventList,
    IEventListTrait, IHostApplication, IHostApplicationTrait, IMessage, IMessageTrait,
    IParamValueQueue, IParamValueQueueTrait, IParameterChanges, IParameterChangesTrait, ParamID,
    ParamValue, String128, TChar,
};
use vst3::Steinberg::{
    int32, int64, kInvalidArgument, kNoInterface, kResultFalse, kResultOk, kResultTrue, tresult,
    uint32, FIDString, IBStream, IBStreamTrait, IPlugFrame, IPlugFrameTrait, IPlugView, TBool,
    ViewRect, TUID,
};
use vst3::{Class, ComWrapper, Interface};

use super::strings::to_char16;
use crate::instance::PluginEvent;

// ---------------------------------------------------------------------------------------------
// IBStream
// ---------------------------------------------------------------------------------------------

/// A growable in-memory stream for plugin state.
pub struct MemoryStream {
    data: RefCell<Vec<u8>>,
    pos: Cell<usize>,
}

impl MemoryStream {
    pub fn new(data: Vec<u8>) -> ComWrapper<MemoryStream> {
        ComWrapper::new(MemoryStream {
            data: RefCell::new(data),
            pos: Cell::new(0),
        })
    }

    pub fn empty() -> ComWrapper<MemoryStream> {
        Self::new(Vec::new())
    }

    /// Copies out everything written so far.
    pub fn bytes(&self) -> Vec<u8> {
        self.data.borrow().clone()
    }

    pub fn rewind(&self) {
        self.pos.set(0);
    }
}

impl Class for MemoryStream {
    type Interfaces = (IBStream,);
}

impl IBStreamTrait for MemoryStream {
    unsafe fn read(&self, buffer: *mut c_void, num_bytes: int32, read: *mut int32) -> tresult {
        if buffer.is_null() || num_bytes < 0 {
            return kInvalidArgument;
        }
        let data = self.data.borrow();
        let pos = self.pos.get().min(data.len());
        let n = (num_bytes as usize).min(data.len() - pos);
        // SAFETY: the plugin promises `buffer` has room for `num_bytes`; we copy at most that.
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr().add(pos), buffer as *mut u8, n);
            if !read.is_null() {
                *read = n as int32;
            }
        }
        self.pos.set(pos + n);
        kResultOk
    }

    unsafe fn write(&self, buffer: *mut c_void, num_bytes: int32, written: *mut int32) -> tresult {
        if buffer.is_null() || num_bytes < 0 {
            return kInvalidArgument;
        }
        let n = num_bytes as usize;
        let mut data = self.data.borrow_mut();
        let pos = self.pos.get();
        if data.len() < pos + n {
            data.resize(pos + n, 0);
        }
        // SAFETY: the plugin promises `buffer` holds `num_bytes` readable bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(buffer as *const u8, data.as_mut_ptr().add(pos), n);
            if !written.is_null() {
                *written = n as int32;
            }
        }
        self.pos.set(pos + n);
        kResultOk
    }

    unsafe fn seek(&self, pos: int64, mode: int32, result: *mut int64) -> tresult {
        const SEEK_SET: int32 = 0;
        const SEEK_CUR: int32 = 1;
        const SEEK_END: int32 = 2;
        let len = self.data.borrow().len() as int64;
        let target = match mode {
            SEEK_SET => pos,
            SEEK_CUR => self.pos.get() as int64 + pos,
            SEEK_END => len + pos,
            _ => return kInvalidArgument,
        };
        if target < 0 {
            return kInvalidArgument;
        }
        self.pos.set(target as usize);
        if !result.is_null() {
            // SAFETY: checked non-null; the plugin supplies a valid out-pointer.
            unsafe { *result = target };
        }
        kResultOk
    }

    unsafe fn tell(&self, pos: *mut int64) -> tresult {
        if pos.is_null() {
            return kInvalidArgument;
        }
        // SAFETY: checked non-null.
        unsafe { *pos = self.pos.get() as int64 };
        kResultOk
    }
}

// ---------------------------------------------------------------------------------------------
// IHostApplication, IMessage, IAttributeList
// ---------------------------------------------------------------------------------------------

/// The host context passed to `IPluginBase::initialize`.
pub struct HostApplication {
    name: String,
}

impl HostApplication {
    pub fn new(name: &str) -> ComWrapper<HostApplication> {
        ComWrapper::new(HostApplication {
            name: name.to_string(),
        })
    }
}

impl Class for HostApplication {
    type Interfaces = (IHostApplication,);
}

impl IHostApplicationTrait for HostApplication {
    unsafe fn getName(&self, name: *mut String128) -> tresult {
        if name.is_null() {
            return kInvalidArgument;
        }
        // SAFETY: checked non-null; String128 is a plain array.
        to_char16(&self.name, unsafe { &mut *name });
        kResultOk
    }

    unsafe fn createInstance(
        &self,
        cid: *mut TUID,
        iid: *mut TUID,
        obj: *mut *mut c_void,
    ) -> tresult {
        if cid.is_null() || iid.is_null() || obj.is_null() {
            return kInvalidArgument;
        }
        // SAFETY: all three pointers were checked; ids are 16 bytes.
        unsafe {
            *obj = std::ptr::null_mut();
            let cid = &*(cid as *const [u8; 16]);
            let iid = &*(iid as *const [u8; 16]);
            if *cid == IMessage::IID && *iid == IMessage::IID {
                let message = HostMessage::new().to_com_ptr::<IMessage>().unwrap();
                *obj = message.into_raw() as *mut c_void;
                return kResultOk;
            }
            if *cid == IAttributeList::IID && *iid == IAttributeList::IID {
                let list = HostAttributeList::new()
                    .to_com_ptr::<IAttributeList>()
                    .unwrap();
                *obj = list.into_raw() as *mut c_void;
                return kResultOk;
            }
        }
        kNoInterface
    }
}

/// A message plugins use to talk between their processor and controller halves.
pub struct HostMessage {
    id: RefCell<Option<std::ffi::CString>>,
    attributes: ComWrapper<HostAttributeList>,
}

impl HostMessage {
    pub fn new() -> ComWrapper<HostMessage> {
        ComWrapper::new(HostMessage {
            id: RefCell::new(None),
            attributes: HostAttributeList::new(),
        })
    }
}

impl Class for HostMessage {
    type Interfaces = (IMessage,);
}

impl IMessageTrait for HostMessage {
    unsafe fn getMessageID(&self) -> FIDString {
        match &*self.id.borrow() {
            // The CString's buffer is stable until the id is replaced.
            Some(id) => id.as_ptr(),
            None => std::ptr::null(),
        }
    }

    unsafe fn setMessageID(&self, id: FIDString) {
        let new = if id.is_null() {
            None
        } else {
            // SAFETY: a non-null FIDString is a NUL-terminated C string.
            Some(unsafe { CStr::from_ptr(id) }.to_owned())
        };
        *self.id.borrow_mut() = new;
    }

    unsafe fn getAttributes(&self) -> *mut IAttributeList {
        // Borrowed pointer: the SDK's own HostMessage does not add a reference here either.
        self.attributes
            .as_com_ref::<IAttributeList>()
            .unwrap()
            .as_ptr()
    }
}

enum AttrValue {
    Int(i64),
    Float(f64),
    String(Vec<TChar>),
    Binary(Vec<u8>),
}

/// Typed key/value storage carried by a [`HostMessage`].
pub struct HostAttributeList {
    values: RefCell<HashMap<Vec<u8>, AttrValue>>,
}

impl HostAttributeList {
    pub fn new() -> ComWrapper<HostAttributeList> {
        ComWrapper::new(HostAttributeList {
            values: RefCell::new(HashMap::new()),
        })
    }

    unsafe fn key(id: AttrID) -> Option<Vec<u8>> {
        if id.is_null() {
            None
        } else {
            // SAFETY: a non-null AttrID is a NUL-terminated C string.
            Some(unsafe { CStr::from_ptr(id) }.to_bytes().to_vec())
        }
    }
}

impl Class for HostAttributeList {
    type Interfaces = (IAttributeList,);
}

impl IAttributeListTrait for HostAttributeList {
    unsafe fn setInt(&self, id: AttrID, value: int64) -> tresult {
        let Some(key) = (unsafe { Self::key(id) }) else {
            return kInvalidArgument;
        };
        self.values.borrow_mut().insert(key, AttrValue::Int(value));
        kResultTrue
    }

    unsafe fn getInt(&self, id: AttrID, value: *mut int64) -> tresult {
        let Some(key) = (unsafe { Self::key(id) }) else {
            return kInvalidArgument;
        };
        match self.values.borrow().get(&key) {
            Some(AttrValue::Int(v)) if !value.is_null() => {
                // SAFETY: checked non-null.
                unsafe { *value = *v };
                kResultTrue
            }
            _ => kResultFalse,
        }
    }

    unsafe fn setFloat(&self, id: AttrID, value: f64) -> tresult {
        let Some(key) = (unsafe { Self::key(id) }) else {
            return kInvalidArgument;
        };
        self.values
            .borrow_mut()
            .insert(key, AttrValue::Float(value));
        kResultTrue
    }

    unsafe fn getFloat(&self, id: AttrID, value: *mut f64) -> tresult {
        let Some(key) = (unsafe { Self::key(id) }) else {
            return kInvalidArgument;
        };
        match self.values.borrow().get(&key) {
            Some(AttrValue::Float(v)) if !value.is_null() => {
                // SAFETY: checked non-null.
                unsafe { *value = *v };
                kResultTrue
            }
            _ => kResultFalse,
        }
    }

    unsafe fn setString(&self, id: AttrID, string: *const TChar) -> tresult {
        let Some(key) = (unsafe { Self::key(id) }) else {
            return kInvalidArgument;
        };
        if string.is_null() {
            return kInvalidArgument;
        }
        let mut units = Vec::new();
        // SAFETY: the plugin passes a NUL-terminated UTF-16 string.
        unsafe {
            let mut p = string;
            while *p != 0 {
                units.push(*p);
                p = p.add(1);
            }
        }
        units.push(0);
        self.values
            .borrow_mut()
            .insert(key, AttrValue::String(units));
        kResultTrue
    }

    unsafe fn getString(&self, id: AttrID, string: *mut TChar, size_in_bytes: uint32) -> tresult {
        let Some(key) = (unsafe { Self::key(id) }) else {
            return kInvalidArgument;
        };
        let capacity = size_in_bytes as usize / std::mem::size_of::<TChar>();
        if string.is_null() || capacity == 0 {
            return kInvalidArgument;
        }
        match self.values.borrow().get(&key) {
            Some(AttrValue::String(units)) => {
                let n = units.len().min(capacity);
                // SAFETY: the plugin promises `string` holds `size_in_bytes` bytes.
                unsafe {
                    std::ptr::copy_nonoverlapping(units.as_ptr(), string, n);
                    *string.add(n - 1) = 0;
                }
                kResultTrue
            }
            _ => kResultFalse,
        }
    }

    unsafe fn setBinary(&self, id: AttrID, data: *const c_void, size_in_bytes: uint32) -> tresult {
        let Some(key) = (unsafe { Self::key(id) }) else {
            return kInvalidArgument;
        };
        let bytes = if data.is_null() || size_in_bytes == 0 {
            Vec::new()
        } else {
            // SAFETY: the plugin promises `data` holds `size_in_bytes` readable bytes.
            unsafe { std::slice::from_raw_parts(data as *const u8, size_in_bytes as usize) }
                .to_vec()
        };
        self.values
            .borrow_mut()
            .insert(key, AttrValue::Binary(bytes));
        kResultTrue
    }

    unsafe fn getBinary(
        &self,
        id: AttrID,
        data: *mut *const c_void,
        size_in_bytes: *mut uint32,
    ) -> tresult {
        let Some(key) = (unsafe { Self::key(id) }) else {
            return kInvalidArgument;
        };
        if data.is_null() || size_in_bytes.is_null() {
            return kInvalidArgument;
        }
        match self.values.borrow().get(&key) {
            Some(AttrValue::Binary(bytes)) => {
                // The pointer stays valid until the attribute is overwritten or the list dies,
                // which is the SDK's contract too.
                // SAFETY: both out-pointers were checked.
                unsafe {
                    *data = bytes.as_ptr() as *const c_void;
                    *size_in_bytes = bytes.len() as uint32;
                }
                kResultTrue
            }
            _ => kResultFalse,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// IComponentHandler, IPlugFrame
// ---------------------------------------------------------------------------------------------

/// Notifications from the plugin, queued for the instance to hand out from `poll_events`.
///
/// The queue is lock-free and bounded because plugins are known to call their component handler
/// from threads they should not (including the audio thread).
pub struct Notifications {
    queue: ArrayQueue<PluginEvent>,
    /// Restart flags accumulated since the last poll (see `RestartFlags`).
    pub restart_flags: std::sync::atomic::AtomicI32,
}

impl Notifications {
    pub fn new() -> Arc<Notifications> {
        Arc::new(Notifications {
            queue: ArrayQueue::new(4096),
            restart_flags: std::sync::atomic::AtomicI32::new(0),
        })
    }

    pub fn push(&self, event: PluginEvent) {
        // If the queue is full the plugin is flooding us; dropping the newest is the only
        // option that never blocks.
        let _ = self.queue.push(event);
    }

    pub fn pop(&self) -> Option<PluginEvent> {
        self.queue.pop()
    }
}

/// Receives edits and restart requests from the plugin's edit controller.
pub struct ComponentHandler {
    notifications: Arc<Notifications>,
}

impl ComponentHandler {
    pub fn new(notifications: Arc<Notifications>) -> ComWrapper<ComponentHandler> {
        ComWrapper::new(ComponentHandler { notifications })
    }
}

impl Class for ComponentHandler {
    type Interfaces = (IComponentHandler, IComponentHandler2);
}

impl IComponentHandlerTrait for ComponentHandler {
    unsafe fn beginEdit(&self, id: ParamID) -> tresult {
        self.notifications
            .push(PluginEvent::ParamGestureBegin { id });
        kResultOk
    }

    unsafe fn performEdit(&self, id: ParamID, value: ParamValue) -> tresult {
        self.notifications
            .push(PluginEvent::ParamChanged { id, value });
        kResultOk
    }

    unsafe fn endEdit(&self, id: ParamID) -> tresult {
        self.notifications.push(PluginEvent::ParamGestureEnd { id });
        kResultOk
    }

    unsafe fn restartComponent(&self, flags: int32) -> tresult {
        self.notifications
            .restart_flags
            .fetch_or(flags, std::sync::atomic::Ordering::AcqRel);
        kResultOk
    }
}

impl IComponentHandler2Trait for ComponentHandler {
    unsafe fn setDirty(&self, state: TBool) -> tresult {
        if state != 0 {
            self.notifications.push(PluginEvent::StateDirty);
        }
        kResultOk
    }

    unsafe fn requestOpenEditor(&self, _name: FIDString) -> tresult {
        kResultFalse
    }

    unsafe fn startGroupEdit(&self) -> tresult {
        kResultOk
    }

    unsafe fn finishGroupEdit(&self) -> tresult {
        kResultOk
    }
}

/// Receives resize requests from the plugin's editor view.
pub struct PlugFrame {
    notifications: Arc<Notifications>,
}

impl PlugFrame {
    pub fn new(notifications: Arc<Notifications>) -> ComWrapper<PlugFrame> {
        ComWrapper::new(PlugFrame { notifications })
    }
}

impl Class for PlugFrame {
    type Interfaces = (IPlugFrame,);
}

impl IPlugFrameTrait for PlugFrame {
    unsafe fn resizeView(&self, _view: *mut IPlugView, new_size: *mut ViewRect) -> tresult {
        if new_size.is_null() {
            return kInvalidArgument;
        }
        // SAFETY: checked non-null.
        let rect = unsafe { *new_size };
        let width = (rect.right - rect.left).max(0) as u32;
        let height = (rect.bottom - rect.top).max(0) as u32;
        self.notifications
            .push(PluginEvent::EditorResizeRequest { width, height });
        kResultTrue
    }
}

// ---------------------------------------------------------------------------------------------
// Audio-thread objects
// ---------------------------------------------------------------------------------------------

/// A fixed-capacity `IEventList`.
///
/// Used from one thread at a time (the audio thread, during `process`), which is what makes the
/// interior mutability sound.
pub struct EventList {
    events: UnsafeCell<Vec<vst3::Steinberg::Vst::Event>>,
}

// SAFETY: the list is only ever touched by whichever single thread is running `process`.
unsafe impl Send for EventList {}
unsafe impl Sync for EventList {}

impl EventList {
    pub fn with_capacity(capacity: usize) -> ComWrapper<EventList> {
        ComWrapper::new(EventList {
            events: UnsafeCell::new(Vec::with_capacity(capacity)),
        })
    }

    /// # Safety
    /// Must not be called while the plugin may be using the list on another thread.
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn events(&self) -> &mut Vec<vst3::Steinberg::Vst::Event> {
        // SAFETY: single-threaded access per the type's contract.
        unsafe { &mut *self.events.get() }
    }
}

impl Class for EventList {
    type Interfaces = (IEventList,);
}

impl IEventListTrait for EventList {
    unsafe fn getEventCount(&self) -> int32 {
        // SAFETY: single-threaded access per the type's contract.
        unsafe { self.events() }.len() as int32
    }

    unsafe fn getEvent(&self, index: int32, e: *mut vst3::Steinberg::Vst::Event) -> tresult {
        // SAFETY: single-threaded access per the type's contract.
        let events = unsafe { self.events() };
        match events.get(index as usize) {
            Some(event) if index >= 0 && !e.is_null() => {
                // SAFETY: checked non-null.
                unsafe { *e = *event };
                kResultOk
            }
            _ => kInvalidArgument,
        }
    }

    unsafe fn addEvent(&self, e: *mut vst3::Steinberg::Vst::Event) -> tresult {
        if e.is_null() {
            return kInvalidArgument;
        }
        // SAFETY: single-threaded access per the type's contract.
        let events = unsafe { self.events() };
        if events.len() == events.capacity() {
            return kResultFalse;
        }
        // SAFETY: checked non-null.
        events.push(unsafe { *e });
        kResultOk
    }
}

/// Points per parameter per block. More than this in one block and the tail collapses onto the
/// last slot, which keeps the final value right.
pub const POINTS_PER_QUEUE: usize = 32;

/// A fixed-capacity `IParamValueQueue`.
pub struct ParamValueQueue {
    id: Cell<ParamID>,
    points: UnsafeCell<Vec<(int32, ParamValue)>>,
}

// SAFETY: as for `EventList`.
unsafe impl Send for ParamValueQueue {}
unsafe impl Sync for ParamValueQueue {}

impl ParamValueQueue {
    fn new() -> ComWrapper<ParamValueQueue> {
        ComWrapper::new(ParamValueQueue {
            id: Cell::new(0),
            points: UnsafeCell::new(Vec::with_capacity(POINTS_PER_QUEUE)),
        })
    }

    #[allow(clippy::mut_from_ref)]
    unsafe fn points(&self) -> &mut Vec<(int32, ParamValue)> {
        // SAFETY: single-threaded access per the type's contract.
        unsafe { &mut *self.points.get() }
    }

    /// Inserts a point keeping the queue sorted by offset. Returns its index.
    unsafe fn add(&self, offset: int32, value: ParamValue) -> usize {
        // SAFETY: single-threaded access per the type's contract.
        let points = unsafe { self.points() };
        if let Some(existing) = points.iter().position(|p| p.0 == offset) {
            points[existing].1 = value;
            return existing;
        }
        if points.len() == points.capacity() {
            let last = points.len() - 1;
            points[last] = (points[last].0.max(offset), value);
            return last;
        }
        let at = points
            .iter()
            .position(|p| p.0 > offset)
            .unwrap_or(points.len());
        points.insert(at, (offset, value));
        at
    }

    /// The final value in this block, if any point was written.
    pub unsafe fn last(&self) -> Option<(int32, ParamValue)> {
        // SAFETY: single-threaded access per the type's contract.
        unsafe { self.points() }.last().copied()
    }

    pub fn id(&self) -> ParamID {
        self.id.get()
    }
}

impl Class for ParamValueQueue {
    type Interfaces = (IParamValueQueue,);
}

impl IParamValueQueueTrait for ParamValueQueue {
    unsafe fn getParameterId(&self) -> ParamID {
        self.id.get()
    }

    unsafe fn getPointCount(&self) -> int32 {
        // SAFETY: single-threaded access per the type's contract.
        unsafe { self.points() }.len() as int32
    }

    unsafe fn getPoint(&self, index: int32, offset: *mut int32, value: *mut ParamValue) -> tresult {
        // SAFETY: single-threaded access per the type's contract.
        let points = unsafe { self.points() };
        match points.get(index as usize) {
            Some(&(o, v)) if index >= 0 && !offset.is_null() && !value.is_null() => {
                // SAFETY: both out-pointers were checked.
                unsafe {
                    *offset = o;
                    *value = v;
                }
                kResultOk
            }
            _ => kResultFalse,
        }
    }

    unsafe fn addPoint(&self, offset: int32, value: ParamValue, index: *mut int32) -> tresult {
        // SAFETY: single-threaded access per the type's contract.
        let at = unsafe { self.add(offset, value) };
        if !index.is_null() {
            // SAFETY: checked non-null.
            unsafe { *index = at as int32 };
        }
        kResultOk
    }
}

/// A fixed pool of parameter queues exposed as `IParameterChanges`.
pub struct ParamChanges {
    queues: Vec<ComWrapper<ParamValueQueue>>,
    used: Cell<usize>,
}

// SAFETY: as for `EventList`.
unsafe impl Send for ParamChanges {}
unsafe impl Sync for ParamChanges {}

impl ParamChanges {
    /// `max_params` is how many distinct parameters can change within one block.
    pub fn with_capacity(max_params: usize) -> ComWrapper<ParamChanges> {
        ComWrapper::new(ParamChanges {
            queues: (0..max_params).map(|_| ParamValueQueue::new()).collect(),
            used: Cell::new(0),
        })
    }

    /// Empties every queue. Call at the start of each block.
    pub fn clear(&self) {
        for queue in &self.queues[..self.used.get()] {
            // SAFETY: single-threaded access per the type's contract.
            unsafe { queue.points() }.clear();
        }
        self.used.set(0);
    }

    /// Queue for `id`, creating it from the pool if needed. `None` when the pool is exhausted.
    fn queue_for(&self, id: ParamID) -> Option<(usize, &ComWrapper<ParamValueQueue>)> {
        let used = self.used.get();
        if let Some(i) = self.queues[..used].iter().position(|q| q.id.get() == id) {
            return Some((i, &self.queues[i]));
        }
        if used == self.queues.len() {
            // More distinct parameters changed in one block than the pool holds: the extra
            // ones are dropped for this block.
            return None;
        }
        let queue = &self.queues[used];
        queue.id.set(id);
        self.used.set(used + 1);
        Some((used, queue))
    }

    /// Adds a point for `id` at `offset`. Returns false if the pool is exhausted.
    pub fn add(&self, id: ParamID, offset: int32, value: ParamValue) -> bool {
        match self.queue_for(id) {
            Some((_, queue)) => {
                // SAFETY: single-threaded access per the type's contract.
                unsafe { queue.add(offset, value) };
                true
            }
            None => false,
        }
    }

    /// The queues written in this block.
    pub fn used(&self) -> &[ComWrapper<ParamValueQueue>] {
        &self.queues[..self.used.get()]
    }
}

impl Class for ParamChanges {
    type Interfaces = (IParameterChanges,);
}

impl IParameterChangesTrait for ParamChanges {
    unsafe fn getParameterCount(&self) -> int32 {
        self.used.get() as int32
    }

    unsafe fn getParameterData(&self, index: int32) -> *mut IParamValueQueue {
        if index < 0 || index as usize >= self.used.get() {
            return std::ptr::null_mut();
        }
        // Borrowed pointer, as the SDK's ParameterChanges returns.
        self.queues[index as usize]
            .as_com_ref::<IParamValueQueue>()
            .unwrap()
            .as_ptr()
    }

    unsafe fn addParameterData(
        &self,
        id: *const ParamID,
        index: *mut int32,
    ) -> *mut IParamValueQueue {
        if id.is_null() {
            return std::ptr::null_mut();
        }
        // SAFETY: checked non-null.
        let id = unsafe { *id };
        match self.queue_for(id) {
            Some((i, queue)) => {
                if !index.is_null() {
                    // SAFETY: checked non-null.
                    unsafe { *index = i as int32 };
                }
                queue.as_com_ref::<IParamValueQueue>().unwrap().as_ptr()
            }
            None => std::ptr::null_mut(),
        }
    }
}
