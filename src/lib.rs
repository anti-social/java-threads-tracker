use std::collections::HashMap;
use std::ffi::{c_char, CStr};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::{Arc, RwLock};
use std::sync::atomic::{AtomicI64, AtomicPtr, Ordering};
use std::thread::spawn;
use std::time::Duration;

use dlopen::symbor::{Library, Ref, SymBorApi};
#[macro_use]
extern crate dlopen_derive;

use format_bytes::format_bytes;

use jvmti::agent::Agent;
use jvmti::context::static_context;
use jvmti::environment::Environment;
use jvmti::environment::jni::JNI;
use jvmti::environment::jvmti::JVMTI;
use jvmti::error::translate_error;
use jvmti::native::{JavaVMPtr, JNIEnvPtr, JVMTIEnvPtr, MutString, VoidPtr};
use jvmti::native::jvmti_native::jvmtiEnv;
use jvmti::options::Options;
use jvmti::thread::Thread;

use lazy_static::lazy_static;

use libc::c_void;

use log::{debug, error, warn};

use tokio::io::AsyncWriteExt;
use tokio::net::UnixListener;
use tokio::sync::{broadcast, Mutex};
use tokio_stream::StreamExt;


static OSTHREAD_FIELD_OFFSET: AtomicI64 = AtomicI64::new(-1);
static OS_THREAD_ID_FIELD_OFFSET: AtomicI64 = AtomicI64::new(-1);

lazy_static! {
    static ref CHANNEL_TX: AtomicPtr<broadcast::Sender<ThreadEvent>> = AtomicPtr::default();
}

#[no_mangle]
#[allow(non_snake_case, unused_variables)]
pub extern "C" fn Agent_OnLoad(
    vm: JavaVMPtr,
    options: MutString,
    reserved: VoidPtr,
) -> i32 {
    env_logger::init();
    debug!("Starting java threads tracker agent");

    if !parse_vm_structs() {
        return 0;
    }

    if options.is_null() {
        error!("Missing options");
        return 0;
    }
    let sock_path = match unsafe { CStr::from_ptr(options) }.to_str() {
        Ok(s) => s,
        Err(e) => {
            error!("Error when parsing options: {}", e);
            return 0;
        }
    };
    if sock_path.is_empty() {
        println!("java-threads-tracker: Missing required socket path option");
        return 0;
    }
    let sock_path = PathBuf::from(sock_path);

    spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime")
            .block_on(
                process_loop(&sock_path)
            );
    });

    let mut agent = Agent::new(vm);
    agent.on_vm_init(Some(on_vm_init));
    agent.on_thread_start(Some(|env, thread| on_thread(env, thread, true)));
    agent.on_thread_end(Some(|env, thread| on_thread(env, thread, false)));
    agent.update();

    0
}

#[derive(SymBorApi)]
struct VMSymbols<'a> {
    #[dlopen_name="gHotSpotVMStructs"]
    structs: Ref<'a, usize>,
    #[dlopen_name="gHotSpotVMStructEntryArrayStride"]
    stride: Ref<'a, usize>,
    #[dlopen_name="gHotSpotVMStructEntryTypeNameOffset"]
    type_offset: Ref<'a, usize>,
    #[dlopen_name="gHotSpotVMStructEntryFieldNameOffset"]
    field_offset: Ref<'a, usize>,
    #[dlopen_name="gHotSpotVMStructEntryOffsetOffset"]
    offset_offset: Ref<'a, usize>,
}

fn parse_vm_structs() -> bool {
    match Library::open("libjvm.so") {
        Ok(libjvm) => {
            debug!("libjvm.so successfully opened");
            let vm_symbols = match unsafe{ VMSymbols::load(&libjvm) } {
                Ok(vm_symbols) => vm_symbols,
                Err(e) => {
                    eprintln!("Cannot load VM symbols: {}", e);
                    return false;
                }
            };
            debug!("VM structs address: 0x{:x}", *vm_symbols.structs);
            debug!("VM entry stride: 0x{:x}", *vm_symbols.stride);
            debug!("VM type offset: 0x{:x}", *vm_symbols.type_offset);
            debug!("VM field offset: 0x{:x}", *vm_symbols.field_offset);
            debug!("VM offset offset: 0x{:x}", *vm_symbols.offset_offset);

            let mut cur_entry_addr = *vm_symbols.structs;
            loop {
                let entry_type_ptr = unsafe {
                    *((cur_entry_addr + *vm_symbols.type_offset) as *const *const c_char)
                };
                let field_name_ptr = unsafe {
                    *((cur_entry_addr + *vm_symbols.field_offset) as *const *const c_char)
                };
                if entry_type_ptr.is_null() || field_name_ptr.is_null() {
                    break;
                }
                let entry_type = unsafe { CStr::from_ptr(entry_type_ptr) }.to_string_lossy();
                let field_name = unsafe { CStr::from_ptr(field_name_ptr) }.to_string_lossy();
                match (entry_type.as_ref(), field_name.as_ref()) {
                    ("JavaThread", "_osthread") => {
                        let osthread_field_offset_addr = cur_entry_addr + *vm_symbols.offset_offset;
                        let osthread_field_offset = unsafe { *(osthread_field_offset_addr as *const i32) };
                        debug!("Found JavaThread._osthread offset: 0x{:x}", osthread_field_offset);
                        OSTHREAD_FIELD_OFFSET.store(osthread_field_offset as i64, Ordering::Release);
                    }
                    ("OSThread", "_thread_id") => {
                        let os_thread_id_field_offset_addr = cur_entry_addr + *vm_symbols.offset_offset;
                        let os_thread_id_field_offset = unsafe { *(os_thread_id_field_offset_addr as *const i32) };
                        debug!("Found OSThread._thread_id offset: 0x{:x}", os_thread_id_field_offset);
                        OS_THREAD_ID_FIELD_OFFSET.store(os_thread_id_field_offset as i64, Ordering::Release);
                    }
                    (_, _) => {}
                }
                cur_entry_addr += *vm_symbols.stride;
            }
        }
        Err(e) => {
            error!("Missing libjvm.so library: {}", e);
            return false;
        }
    }

    true
}

async fn process_loop(sock_path: &Path) {
    let mut all_threads = Mutex::new(HashMap::<i64, ThreadEvent>::new());

    if sock_path.exists() {
        std::fs::remove_file(sock_path);
    }
    let sock_listener = match UnixListener::bind(sock_path) {
        Ok(listener) => listener,
        Err(e) => {
            error!("Error when binding to socket: {}", e);
            return;
        }
    };

    let (mut tx, mut rx) = broadcast::channel::<ThreadEvent>(64);
    CHANNEL_TX.store(&mut tx, Ordering::Release);

    let mut clients = Mutex::new(vec!());
    loop {
        // We use tokio mainly to be able to multiplex accepting new connections and sending data to clients
        tokio::select! {
            conn = sock_listener.accept() => {
                match conn {
                    Ok((mut stream, _)) => {
                        debug!("Client connected");
                        {
                            let all_threads = all_threads.lock().await;
                            for (_, t) in (*all_threads).iter() {
                                stream.write_all(&t.serialize()).await;
                            }
                            stream.flush().await;
                        }

                        let mut clients = clients.lock().await;
                        (*clients).push(stream);
                    }
                    Err(e) => {
                        warn!("Error when accepting a connection on a socket")
                    }
                }
            }
            thread_event = rx.recv() => {
                match thread_event {
                    Ok(t) => {
                        {
                            let mut all_threads = all_threads.lock().await;
                            if t.is_started {
                                all_threads.insert(t.id, t.clone());
                            } else {
                                all_threads.remove(&t.id);
                            }
                        }

                        let mut clients = clients.lock().await;
                        let mut remove_client_ixs = vec!();
                        for (client_ix, client) in (*clients).iter_mut().enumerate() {
                            // TODO: add timeout for client operations
                            if let Err(e) = client.write_all(&t.serialize()).await {
                                remove_client_ixs.push(client_ix);
                                continue;
                            }
                            if let Err(e) = client.flush().await {
                                remove_client_ixs.push(client_ix);
                            }
                        }
                        for client_ix in remove_client_ixs.iter().rev() {
                            clients.remove(*client_ix);
                            debug!("Client disconnected: {:?}", &client_ix);
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!("Channel is overflowed, {} messages was deleted", n);
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        break;
                    }
                }
            }
        }
    }
}

pub fn on_vm_init(env: Environment, thread: Thread) {
    let tx = match unsafe { CHANNEL_TX.load(Ordering::Acquire).as_ref() } {
        Some(tx) => tx,
        None => return,
    };

    let threads = match env.get_all_threads() {
        Ok(all_threads) => {
            for thread_id in all_threads {
                match env.get_thread_info(&thread_id) {
                    Ok(thread) => {
                        let os_thread_id = match get_os_thread_id(&env, &thread) {
                            Some(tid) => tid,
                            None => continue,
                        };
                        let java_thread_class = env.get_object_class(&thread.id.native_id);
                        let thread_get_id_method = env.get_method_id(
                            &java_thread_class, "getId", "()J"
                        );
                        let thread_id = env.call_long_method(&thread.id.native_id, &thread_get_id_method);
                        tx.send(ThreadEvent {
                            id: thread_id,
                            tid: os_thread_id,
                            name: thread.name,
                            is_started: true,
                        });
                    },
                    Err(e) => warn!("Cannot receive thread info: {}", translate_error(&e)),
                }
            }
        }
        Err(e) => warn!("Cannot receive all threads: {}", translate_error(&e)),
    };

}

pub fn on_thread(env: Environment, thread: Thread, is_started: bool) {
    let os_thread_id = match get_os_thread_id(&env, &thread) {
        Some(tid) => tid,
        None => return,
    };

    let tx = CHANNEL_TX.load(Ordering::Acquire);
    if let Some(tx) = unsafe { tx.as_ref() } {
        let java_thread_class = env.get_object_class(&thread.id.native_id);
        let thread_get_id_method = env.get_method_id(&java_thread_class, "getId", "()J");
        let thread_id = env.call_long_method(&thread.id.native_id, &thread_get_id_method);
        let thread_event = ThreadEvent {
            id: thread_id,
            tid: os_thread_id,
            name: thread.name,
            is_started,
        };
        let send_result = tx.send(thread_event);
        if let Err(e) = send_result {
            warn!("Cannot send thread event, possibly there is no receiver");
        }
    }
    static_context().thread_start(&thread.id);
}

fn get_os_thread_id(env: &Environment, thread: &Thread) -> Option<u32> {
    let java_thread = thread.id.native_id;
    let java_thread_class = env.get_object_class(&java_thread);
    let eetop_field_id =  env.get_field_id(&java_thread_class, "eetop", "J");
    let eetop_addr = env.get_long_field(&java_thread, eetop_field_id);
    let osthread_field_offset = OSTHREAD_FIELD_OFFSET.load(Ordering::Acquire);
    if osthread_field_offset < 0 {
        return None;
    }
    let os_thread_id_field_offset = OS_THREAD_ID_FIELD_OFFSET.load(Ordering::Acquire) as isize;
    if os_thread_id_field_offset < 0 {
        return None;
    }

    let os_thread_addr = unsafe {
        *((eetop_addr + osthread_field_offset) as *const isize)
    };
    let os_thread_id = unsafe {
        *((os_thread_addr + os_thread_id_field_offset) as *const i32)
    };

    return Some(os_thread_id as u32);
}

#[derive(Debug, Clone)]
struct ThreadEvent {
    id: i64,
    tid: u32,
    name: String,
    is_started: bool,
}

impl ThreadEvent {
    fn serialize(&self) -> Vec<u8> {
        let sign = if (self.is_started) { "+" } else { "-" };
        format_bytes!(b"{}{}: {}\n", sign.as_bytes(), self.tid, self.name.as_bytes())
    }
}
