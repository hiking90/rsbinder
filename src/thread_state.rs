use std::sync::Arc;
use std::os::unix::io::AsRawFd;
use std::cell::RefCell;
use std::io;
use nix;
use crate::{*, sys::binder};

thread_local! {
    pub static THREAD_STATE: RefCell<ThreadState> = RefCell::new(ThreadState::new());
}

const RETURN_STRINGS: [&'static str; 21] =
[
    "BR_ERROR",
    "BR_OK",
    "BR_TRANSACTION",
    "BR_REPLY",
    "BR_ACQUIRE_RESULT",
    "BR_DEAD_REPLY",
    "BR_TRANSACTION_COMPLETE",
    "BR_INCREFS",
    "BR_ACQUIRE",
    "BR_RELEASE",
    "BR_DECREFS",
    "BR_ATTEMPT_ACQUIRE",
    "BR_NOOP",
    "BR_SPAWN_LOOPER",
    "BR_FINISHED",
    "BR_DEAD_BINDER",
    "BR_CLEAR_DEATH_NOTIFICATION_DONE",
    "BR_FAILED_REPLY",
    "BR_FROZEN_REPLY",
    "BR_ONEWAY_SPAM_SUSPECT",
    "BR_TRANSACTION_SEC_CTX",
];

fn return_to_str(cmd: std::os::raw::c_uint) -> &'static str {
    let idx: usize = (cmd & binder::_IOC_NRMASK) as _;

    if idx < RETURN_STRINGS.len() {
        RETURN_STRINGS[idx]
    } else {
        "Unknown BR_ return"
    }
}

const COMMAND_STRINGS: [&'static str; 17] =
[
    "BC_TRANSACTION",
    "BC_REPLY",
    "BC_ACQUIRE_RESULT",
    "BC_FREE_BUFFER",
    "BC_INCREFS",
    "BC_ACQUIRE",
    "BC_RELEASE",
    "BC_DECREFS",
    "BC_INCREFS_DONE",
    "BC_ACQUIRE_DONE",
    "BC_ATTEMPT_ACQUIRE",
    "BC_REGISTER_LOOPER",
    "BC_ENTER_LOOPER",
    "BC_EXIT_LOOPER",
    "BC_REQUEST_DEATH_NOTIFICATION",
    "BC_CLEAR_DEATH_NOTIFICATION",
    "BC_DEAD_BINDER_DONE"
];

fn command_to_str(cmd: std::os::raw::c_uint) -> &'static str {
    let idx: usize = (cmd & 0xFF) as _;

    if idx < COMMAND_STRINGS.len() {
        COMMAND_STRINGS[idx]
    } else {
        "Unknown BC_ command"
    }
}

const WORK_SOURCE_PROPAGATED_BIT_INDEX: i64 = 32;
const UNSET_WORK_SOURCE: i32 = -1;

#[derive(Debug, Clone, Copy)]
struct TransactionState {
    calling_pid: binder::pid_t,
    calling_sid: *const u8,
    calling_uid: binder::uid_t,
    strict_mode_policy: i32,
    last_transaction_binder_flags: u32,
    work_source: binder::uid_t,
    propagate_work_source: bool,
}

impl TransactionState {
    fn from_transaction_data(data: &binder::binder_transaction_data_secctx) -> Self {
        TransactionState {
            calling_pid: data.transaction_data.sender_pid,
            calling_sid: data.secctx as _,
            calling_uid: data.transaction_data.sender_euid,
            strict_mode_policy: 0,
            last_transaction_binder_flags: data.transaction_data.flags,
            work_source: 0,
            propagate_work_source: false,
        }
    }
}


pub struct ThreadState {
    in_parcel: Parcel,
    out_parcel: Parcel,
    transaction: Option<TransactionState>,
    strict_mode_policy: i32,
}


impl ThreadState {
    fn new() -> Self {
        ThreadState {
            in_parcel: Parcel::new(),
            out_parcel: Parcel::new(),
            transaction: None,
            strict_mode_policy: 0,
        }
    }

    pub fn set_strict_mode_policy(&mut self, policy: i32) {
        self.strict_mode_policy = policy;
    }

    pub fn strict_mode_policy(&self) -> i32 {
        self.strict_mode_policy
    }

    pub fn setup_polling(&mut self) -> Result<std::os::unix::io::RawFd> {
        self.out_parcel.as_writable().write::<u32>(&binder::BC_ENTER_LOOPER);
        self.flash_commands()?;
        Ok(ProcessState::as_self().read().unwrap().as_raw_fd())
    }

    fn flash_commands(&mut self) -> Result<()> {
        self.talk_with_driver(false)?;
        if self.out_parcel.len() > 0 {
            self.talk_with_driver(false)?;
        }

        if self.out_parcel.len() > 0 {
            log::warn!("self.out_parcel.len() > 0 after flash_commands()");
        }

        Ok(())
    }

    pub fn handle_commands(&mut self) -> Result<()>{
        while {
            self.get_and_execute_command()?;
            self.in_parcel.len() != 0
        } {
            self.flash_commands()?;
        }
        Ok(())
    }

    fn get_and_execute_command(&mut self) -> Result<()> {
        self.talk_with_driver(true)?;

        if self.in_parcel.len() < std::mem::size_of::<i32>() {
            return Ok(())
        }

        self.in_parcel.dump();
        let cmd = self.in_parcel.as_readable().read::<i32>().unwrap_or(0);
        self.execute_command(cmd)?;

        Ok(())
    }

    fn execute_command(&mut self, cmd: i32) -> Result<()> {
        let cmd: std::os::raw::c_uint = cmd as _;

        println!("execute_command: {} {:?}", cmd, return_to_str(cmd));

        match cmd {
            binder::BR_ERROR => {
                let other = self.in_parcel.as_readable().read::<i32>()?;
                return Err(Error::from(other));
            }
            binder::BR_OK => {}

            binder::BR_TRANSACTION_SEC_CTX |
            binder::BR_TRANSACTION => {
                let tr_secctx = if cmd == binder::BR_TRANSACTION_SEC_CTX {
                    self.in_parcel.as_readable().read::<binder::binder_transaction_data_secctx>()?
                } else {
                    binder::binder_transaction_data_secctx {
                        transaction_data: self.in_parcel.as_readable().read::<binder::binder_transaction_data>()?,
                        secctx: 0,
                    }
                };

                let mut reader = unsafe {
                    let tr = &tr_secctx.transaction_data;

                    Parcel::from_ipc_parts(tr.data.ptr.buffer as _, tr.data_size as _,
                        tr.data.ptr.offsets as _, (tr.offsets_size as usize) / std::mem::size_of::<binder::binder_size_t>())
                };

                // TODO: Skip now, because if below implmentation is mandatory.
                // const void* origServingStackPointer = mServingStackPointer;
                // mServingStackPointer = &origServingStackPointer; // anything on the stack

                let transaction_old = self.transaction;

                self.clear_calling_work_source();
                self.clear_propagate_work_source();

                self.transaction = Some(TransactionState::from_transaction_data(&tr_secctx));

                let mut reply = Parcel::new();

                unsafe {
                    let target_ptr = tr_secctx.transaction_data.target.ptr;
                    if target_ptr != 0 {
                        todo!("need to call BBinder->transact()")
                        // let weak_ref: *mut ref_base::WeakRef = target_ptr as _;
                        // let mut ref_base = ref_base::RefBase::<ref_base::RemoteRef>::from_raw(target_ptr as _);
                        // if ref_base.attempt_inc_strong() {
                        //     todo!("need to call BBinder->transact()")
                        // }
                //     if (reinterpret_cast<RefBase::weakref_type*>(
                //             tr.target.ptr)->attemptIncStrong(this)) {
                //         error = reinterpret_cast<BBinder*>(tr.cookie)->transact(tr.code, buffer,
                //                 &reply, tr.flags);
                //         reinterpret_cast<BBinder*>(tr.cookie)->decStrong(this);
                //     } else {
                //         error = UNKNOWN_TRANSACTION;
                //     }

                    } else {
                        if let Some(context) = ProcessState::as_self().read().unwrap().context_manager() {
                            context.transact(tr_secctx.transaction_data.code, &mut reader, &mut reply)?;
                        }
                    }
                }

                let flags = tr_secctx.transaction_data.flags;
                if (flags & sys::transaction_flags_TF_ONE_WAY) == 0 {
                    let flags = flags & sys::transaction_flags_TF_CLEAR_BUF;
                    self.write_transaction_data(binder::BC_REPLY as _, flags, u32::MAX, 0, &mut reply)?;
                    reply.set_len(0);
                    self.wait_for_response(&mut reply)?;
                }

                self.transaction = transaction_old;
            }

            binder::BR_REPLY => {
                todo!("execute_command - BR_REPLY");
            }
            binder::BR_ACQUIRE_RESULT => {
                todo!("execute_command - BR_ACQUIRE_RESULT");
            }
            binder::BR_DEAD_REPLY => {
                todo!("execute_command - BR_DEAD_REPLY");
            }
            binder::BR_TRANSACTION_COMPLETE => {
                todo!("execute_command - BR_TRANSACTION_COMPLETE");
            }
            binder::BR_INCREFS => {
                todo!("execute_command - BR_INCREFS");
        // refs = (RefBase::weakref_type*)mIn.readPointer();
        // obj = (BBinder*)mIn.readPointer();
        // refs->incWeak(mProcess.get());
        // mOut.writeInt32(BC_INCREFS_DONE);
        // mOut.writePointer((uintptr_t)refs);
        // mOut.writePointer((uintptr_t)obj);

            }
            binder::BR_ACQUIRE => {
                todo!("execute_command - BR_ACQUIRE");
        // refs = (RefBase::weakref_type*)mIn.readPointer();
        // obj = (BBinder*)mIn.readPointer();
        // ALOG_ASSERT(refs->refBase() == obj,
        //            "BR_ACQUIRE: object %p does not match cookie %p (expected %p)",
        //            refs, obj, refs->refBase());
        // obj->incStrong(mProcess.get());
        // IF_LOG_REMOTEREFS() {
        //     LOG_REMOTEREFS("BR_ACQUIRE from driver on %p", obj);
        //     obj->printRefs();
        // }
        // mOut.writeInt32(BC_ACQUIRE_DONE);
        // mOut.writePointer((uintptr_t)refs);
        // mOut.writePointer((uintptr_t)obj);

            }
            binder::BR_RELEASE => {
                todo!("execute_command - BR_RELEASE");
        // refs = (RefBase::weakref_type*)mIn.readPointer();
        // obj = (BBinder*)mIn.readPointer();
        // ALOG_ASSERT(refs->refBase() == obj,
        //            "BR_RELEASE: object %p does not match cookie %p (expected %p)",
        //            refs, obj, refs->refBase());
        // IF_LOG_REMOTEREFS() {
        //     LOG_REMOTEREFS("BR_RELEASE from driver on %p", obj);
        //     obj->printRefs();
        // }
        // mPendingStrongDerefs.push(obj);

            }
            binder::BR_DECREFS => {
                todo!("execute_command - BR_DECREFS");
        // refs = (RefBase::weakref_type*)mIn.readPointer();
        // obj = (BBinder*)mIn.readPointer();
        // // NOTE: This assertion is not valid, because the object may no
        // // longer exist (thus the (BBinder*)cast above resulting in a different
        // // memory address).
        // //ALOG_ASSERT(refs->refBase() == obj,
        // //           "BR_DECREFS: object %p does not match cookie %p (expected %p)",
        // //           refs, obj, refs->refBase());
        // mPendingWeakDerefs.push(refs);
            }
            binder::BR_ATTEMPT_ACQUIRE => {
                todo!("execute_command - BR_ATTEMPT_ACQUIRE");
        // refs = (RefBase::weakref_type*)mIn.readPointer();
        // obj = (BBinder*)mIn.readPointer();

        // {
        //     const bool success = refs->attemptIncStrong(mProcess.get());
        //     ALOG_ASSERT(success && refs->refBase() == obj,
        //                "BR_ATTEMPT_ACQUIRE: object %p does not match cookie %p (expected %p)",
        //                refs, obj, refs->refBase());

        //     mOut.writeInt32(BC_ACQUIRE_RESULT);
        //     mOut.writeInt32((int32_t)success);
        // }
            }
            binder::BR_NOOP => {}
            binder::BR_SPAWN_LOOPER => {
                todo!("execute_command - BR_SPAWN_LOOPER");
            }
            binder::BR_FINISHED => {
                todo!("execute_command - BR_FINISHED");
            }
            binder::BR_DEAD_BINDER => {
                todo!("Bexecute_command - R_DEAD_BINDER");
            }
            binder::BR_CLEAR_DEATH_NOTIFICATION_DONE => {
                todo!("execute_command - BR_CLEAR_DEATH_NOTIFICATION_DONE");
            }
            binder::BR_FAILED_REPLY => {
                todo!("execute_command - BR_FAILED_REPLY");
            }
            binder::BR_FROZEN_REPLY => {
                todo!("execute_command - BR_FROZEN_REPLY");
            }
            binder::BR_ONEWAY_SPAM_SUSPECT => {
                todo!("execute_command - BR_ONEWAY_SPAM_SUSPECT");
            }
            _ => {}
        }

        Ok(())
    }

    fn talk_with_driver(&mut self, do_receive: bool) -> Result<()> {
        if ProcessState::as_self().read().unwrap().as_raw_fd() < 0 {
            return Err(error::Error::from(error::ErrorKind::BadFd));
        }

        let need_read = self.in_parcel.is_empty();
        let out_avail = if !do_receive || need_read {
            self.out_parcel.len()
        } else {
            0
        };

        let read_size = if do_receive && need_read {
            self.in_parcel.capacity()
        } else {
            0
        };

        let mut bwr = binder::binder_write_read {
            write_size: out_avail as _,
            write_consumed: 0,
            write_buffer: self.out_parcel.as_mut_ptr() as _,
            read_size: read_size as _,
            read_consumed: 0,
            read_buffer: self.in_parcel.as_mut_ptr() as _,
        };

        if bwr.write_size == 0 && bwr.read_size == 0 {
            return Ok(())
        }

        unsafe {
            loop {
                let res = binder::write_read(ProcessState::as_self().read().unwrap().as_raw_fd(), &mut bwr);
                match res {
                    Ok(_) => break,
                    Err(errno) if errno != nix::errno::Errno::EINTR => {
                        return Err(Error::from(errno));
                    },
                    _ => {}
                }

            }
        }

        if bwr.write_consumed > 0 {
            if bwr.write_consumed < self.out_parcel.len() as _ {
                panic!("Driver did not consume write buffer. consumed: {} of {}",
                    bwr.write_consumed, self.out_parcel.len());
            } else {
                self.out_parcel.set_len(0);
                self.process_post_write_derefs();
            }
        }

        if bwr.read_consumed > 0 {
            self.in_parcel.set_len(bwr.read_consumed as _);
            self.in_parcel.set_data_position(0);
        }

        Ok(())
    }

    fn process_post_write_derefs(&mut self) {

    }

    fn clear_propagate_work_source(&mut self) {
        if let Some(mut state) = self.transaction {
            state.propagate_work_source = false;
        }
    }

    fn clear_calling_work_source(&mut self) {
        self.set_calling_work_source_uid(UNSET_WORK_SOURCE as _);
    }

    fn set_calling_work_source_uid(&mut self, uid: binder::uid_t) -> i64 {
        let token = self.set_calling_work_source_uid_without_propagation(uid);
        if let Some(mut state) = self.transaction {
            state.propagate_work_source = true;
        }
        token
    }

    fn set_calling_work_source_uid_without_propagation(&mut self, uid: binder::uid_t) -> i64 {
        match self.transaction {
            Some(mut state) => {
                let propagated_bit = (state.propagate_work_source as i64) << WORK_SOURCE_PROPAGATED_BIT_INDEX;
                let token = propagated_bit | (state.work_source as i64);
                state.work_source = uid;

                token
            }
            None => {
                0
            }
        }
    }

    fn write_transaction_data(&mut self, cmd: i32, flags: u32, handle: u32, code: u32, data: &mut Parcel) -> Result<()> {
        let tr = sys::binder_transaction_data {
            target: sys::binder_transaction_data__bindgen_ty_1 {
                handle: handle,
                // ptr: 0,
            },
            code: code,
            flags: flags,
            cookie: 0,
            sender_pid: 0,
            sender_euid: 0,
            data_size: data.len() as _,
            offsets_size: 0,
            data: sys::binder_transaction_data__bindgen_ty_2 {
                ptr: sys::binder_transaction_data__bindgen_ty_2__bindgen_ty_1 {
                    buffer: data.as_mut_ptr() as _,
                    offsets: 0,
                },
                // buf: ,  // [__u8; 8usize],
            }
        };

        let mut writer = self.out_parcel.as_writable();
        writer.write::<i32>(&cmd);
        unsafe {
            let ptr = &tr as *const sys::binder_transaction_data;
            writer.write_data(
                std::slice::from_raw_parts(ptr as _, std::mem::size_of::<sys::binder_transaction_data>())
            );
        }

        Ok(())
    }

    fn wait_for_response(&mut self, reply: &mut Parcel) -> Result<()> {
        loop {
            self.talk_with_driver(true)?;
            if self.in_parcel.is_empty() {
                continue;
            }

            let cmd: u32 = self.in_parcel.as_readable().read::<i32>()? as _;
            match cmd {
                binder::BR_ONEWAY_SPAM_SUSPECT => {
                    todo!("wait_for_response - BR_ONEWAY_SPAM_SUSPECT");
                },
                binder::BR_TRANSACTION_COMPLETE => (),
                binder::BR_DEAD_REPLY => {
                    todo!("wait_for_response - BR_DEAD_REPLY");
                },
                binder::BR_FAILED_REPLY => {
                    todo!("wait_for_response - BR_FAILED_REPLY");
                },
                binder::BR_FROZEN_REPLY => {
                    todo!("wait_for_response - BR_FROZEN_REPLY");
                },
                binder::BR_ACQUIRE_RESULT => {
                    todo!("wait_for_response - BR_ACQUIRE_RESULT");
                },
                binder::BR_REPLY => {
                    todo!("wait_for_response - BR_REPLY");
                },
                _ => self.execute_command(cmd as _)?,
            };
        }

        Ok(())
    }

// status_t IPCThreadState::writeTransactionData(int32_t cmd, uint32_t binderFlags,
//     int32_t handle, uint32_t code, const Parcel& data, status_t* statusBuffer)
// {
//     binder_transaction_data tr;

//     tr.target.ptr = 0; /* Don't pass uninitialized stack data to a remote process */
//     tr.target.handle = handle;
//     tr.code = code;
//     tr.flags = binderFlags;
//     tr.cookie = 0;
//     tr.sender_pid = 0;
//     tr.sender_euid = 0;

//     const status_t err = data.errorCheck();
//     if (err == NO_ERROR) {
//         tr.data_size = data.ipcDataSize();
//         tr.data.ptr.buffer = data.ipcData();
//         tr.offsets_size = data.ipcObjectsCount()*sizeof(binder_size_t);
//         tr.data.ptr.offsets = data.ipcObjects();
//     } else if (statusBuffer) {
//         tr.flags |= TF_STATUS_CODE;
//         *statusBuffer = err;
//         tr.data_size = sizeof(status_t);
//         tr.data.ptr.buffer = reinterpret_cast<uintptr_t>(statusBuffer);
//         tr.offsets_size = 0;
//         tr.data.ptr.offsets = 0;
//     } else {
//         return (mLastError = err);
//     }

//     mOut.writeInt32(cmd);
//     mOut.write(&tr, sizeof(tr));

//     return NO_ERROR;
// }

}

// status_t IPCThreadState::setupPolling(int* fd)
// {
//     if (mProcess->mDriverFD < 0) {
//         return -EBADF;
//     }

//     mOut.writeInt32(BC_ENTER_LOOPER);
//     flushCommands();
//     *fd = mProcess->mDriverFD;
//     return 0;
// }




// void IPCThreadState::flushCommands()
// {
//     if (mProcess->mDriverFD < 0)
//         return;
//     talkWithDriver(false);
//     // The flush could have caused post-write refcount decrements to have
//     // been executed, which in turn could result in BC_RELEASE/BC_DECREFS
//     // being queued in mOut. So flush again, if we need to.
//     if (mOut.dataSize() > 0) {
//         talkWithDriver(false);
//     }
//     if (mOut.dataSize() > 0) {
//         ALOGW("mOut.dataSize() > 0 after flushCommands()");
//     }
// }


// status_t IPCThreadState::executeCommand(int32_t cmd)
// {
//     BBinder* obj;
//     RefBase::weakref_type* refs;
//     status_t result = NO_ERROR;

//     switch ((uint32_t)cmd) {
//     case BR_ERROR:
//         result = mIn.readInt32();
//         break;

//     case BR_OK:
//         break;

//     case BR_ACQUIRE:
//         refs = (RefBase::weakref_type*)mIn.readPointer();
//         obj = (BBinder*)mIn.readPointer();
//         ALOG_ASSERT(refs->refBase() == obj,
//                    "BR_ACQUIRE: object %p does not match cookie %p (expected %p)",
//                    refs, obj, refs->refBase());
//         obj->incStrong(mProcess.get());
//         IF_LOG_REMOTEREFS() {
//             LOG_REMOTEREFS("BR_ACQUIRE from driver on %p", obj);
//             obj->printRefs();
//         }
//         mOut.writeInt32(BC_ACQUIRE_DONE);
//         mOut.writePointer((uintptr_t)refs);
//         mOut.writePointer((uintptr_t)obj);
//         break;

//     case BR_RELEASE:
//         refs = (RefBase::weakref_type*)mIn.readPointer();
//         obj = (BBinder*)mIn.readPointer();
//         ALOG_ASSERT(refs->refBase() == obj,
//                    "BR_RELEASE: object %p does not match cookie %p (expected %p)",
//                    refs, obj, refs->refBase());
//         IF_LOG_REMOTEREFS() {
//             LOG_REMOTEREFS("BR_RELEASE from driver on %p", obj);
//             obj->printRefs();
//         }
//         mPendingStrongDerefs.push(obj);
//         break;

//     case BR_INCREFS:
//         refs = (RefBase::weakref_type*)mIn.readPointer();
//         obj = (BBinder*)mIn.readPointer();
//         refs->incWeak(mProcess.get());
//         mOut.writeInt32(BC_INCREFS_DONE);
//         mOut.writePointer((uintptr_t)refs);
//         mOut.writePointer((uintptr_t)obj);
//         break;

//     case BR_DECREFS:
//         refs = (RefBase::weakref_type*)mIn.readPointer();
//         obj = (BBinder*)mIn.readPointer();
//         // NOTE: This assertion is not valid, because the object may no
//         // longer exist (thus the (BBinder*)cast above resulting in a different
//         // memory address).
//         //ALOG_ASSERT(refs->refBase() == obj,
//         //           "BR_DECREFS: object %p does not match cookie %p (expected %p)",
//         //           refs, obj, refs->refBase());
//         mPendingWeakDerefs.push(refs);
//         break;

//     case BR_ATTEMPT_ACQUIRE:
//         refs = (RefBase::weakref_type*)mIn.readPointer();
//         obj = (BBinder*)mIn.readPointer();

//         {
//             const bool success = refs->attemptIncStrong(mProcess.get());
//             ALOG_ASSERT(success && refs->refBase() == obj,
//                        "BR_ATTEMPT_ACQUIRE: object %p does not match cookie %p (expected %p)",
//                        refs, obj, refs->refBase());

//             mOut.writeInt32(BC_ACQUIRE_RESULT);
//             mOut.writeInt32((int32_t)success);
//         }
//         break;

//     case BR_TRANSACTION_SEC_CTX:
//     case BR_TRANSACTION:
//         {
//             binder_transaction_data_secctx tr_secctx;
//             binder_transaction_data& tr = tr_secctx.transaction_data;

//             if (cmd == (int) BR_TRANSACTION_SEC_CTX) {
//                 result = mIn.read(&tr_secctx, sizeof(tr_secctx));
//             } else {
//                 result = mIn.read(&tr, sizeof(tr));
//                 tr_secctx.secctx = 0;
//             }

//             ALOG_ASSERT(result == NO_ERROR,
//                 "Not enough command data for brTRANSACTION");
//             if (result != NO_ERROR) break;

//             Parcel buffer;
//             buffer.ipcSetDataReference(
//                 reinterpret_cast<const uint8_t*>(tr.data.ptr.buffer),
//                 tr.data_size,
//                 reinterpret_cast<const binder_size_t*>(tr.data.ptr.offsets),
//                 tr.offsets_size/sizeof(binder_size_t), freeBuffer);

//             const void* origServingStackPointer = mServingStackPointer;
//             mServingStackPointer = &origServingStackPointer; // anything on the stack

//             const pid_t origPid = mCallingPid;
//             const char* origSid = mCallingSid;
//             const uid_t origUid = mCallingUid;
//             const int32_t origStrictModePolicy = mStrictModePolicy;
//             const int32_t origTransactionBinderFlags = mLastTransactionBinderFlags;
//             const int32_t origWorkSource = mWorkSource;
//             const bool origPropagateWorkSet = mPropagateWorkSource;
//             // Calling work source will be set by Parcel#enforceInterface. Parcel#enforceInterface
//             // is only guaranteed to be called for AIDL-generated stubs so we reset the work source
//             // here to never propagate it.
//             clearCallingWorkSource();
//             clearPropagateWorkSource();

//             mCallingPid = tr.sender_pid;
//             mCallingSid = reinterpret_cast<const char*>(tr_secctx.secctx);
//             mCallingUid = tr.sender_euid;
//             mLastTransactionBinderFlags = tr.flags;

//             // ALOGI(">>>> TRANSACT from pid %d sid %s uid %d\n", mCallingPid,
//             //    (mCallingSid ? mCallingSid : "<N/A>"), mCallingUid);

//             Parcel reply;
//             status_t error;
//             IF_LOG_TRANSACTIONS() {
//                 TextOutput::Bundle _b(alog);
//                 alog << "BR_TRANSACTION thr " << (void*)pthread_self()
//                     << " / obj " << tr.target.ptr << " / code "
//                     << TypeCode(tr.code) << ": " << indent << buffer
//                     << dedent << endl
//                     << "Data addr = "
//                     << reinterpret_cast<const uint8_t*>(tr.data.ptr.buffer)
//                     << ", offsets addr="
//                     << reinterpret_cast<const size_t*>(tr.data.ptr.offsets) << endl;
//             }
//             if (tr.target.ptr) {
//                 // We only have a weak reference on the target object, so we must first try to
//                 // safely acquire a strong reference before doing anything else with it.
//                 if (reinterpret_cast<RefBase::weakref_type*>(
//                         tr.target.ptr)->attemptIncStrong(this)) {
//                     error = reinterpret_cast<BBinder*>(tr.cookie)->transact(tr.code, buffer,
//                             &reply, tr.flags);
//                     reinterpret_cast<BBinder*>(tr.cookie)->decStrong(this);
//                 } else {
//                     error = UNKNOWN_TRANSACTION;
//                 }

//             } else {
//                 error = the_context_object->transact(tr.code, buffer, &reply, tr.flags);
//             }

//             //ALOGI("<<<< TRANSACT from pid %d restore pid %d sid %s uid %d\n",
//             //     mCallingPid, origPid, (origSid ? origSid : "<N/A>"), origUid);

//             if ((tr.flags & TF_ONE_WAY) == 0) {
//                 LOG_ONEWAY("Sending reply to %d!", mCallingPid);
//                 if (error < NO_ERROR) reply.setError(error);

//                 constexpr uint32_t kForwardReplyFlags = TF_CLEAR_BUF;
//                 sendReply(reply, (tr.flags & kForwardReplyFlags));
//             } else {
//                 if (error != OK) {
//                     alog << "oneway function results for code " << tr.code
//                          << " on binder at "
//                          << reinterpret_cast<void*>(tr.target.ptr)
//                          << " will be dropped but finished with status "
//                          << statusToString(error);

//                     // ideally we could log this even when error == OK, but it
//                     // causes too much logspam because some manually-written
//                     // interfaces have clients that call methods which always
//                     // write results, sometimes as oneway methods.
//                     if (reply.dataSize() != 0) {
//                          alog << " and reply parcel size " << reply.dataSize();
//                     }

//                     alog << endl;
//                 }
//                 LOG_ONEWAY("NOT sending reply to %d!", mCallingPid);
//             }

//             mServingStackPointer = origServingStackPointer;
//             mCallingPid = origPid;
//             mCallingSid = origSid;
//             mCallingUid = origUid;
//             mStrictModePolicy = origStrictModePolicy;
//             mLastTransactionBinderFlags = origTransactionBinderFlags;
//             mWorkSource = origWorkSource;
//             mPropagateWorkSource = origPropagateWorkSet;

//             IF_LOG_TRANSACTIONS() {
//                 TextOutput::Bundle _b(alog);
//                 alog << "BC_REPLY thr " << (void*)pthread_self() << " / obj "
//                     << tr.target.ptr << ": " << indent << reply << dedent << endl;
//             }

//         }
//         break;

//     case BR_DEAD_BINDER:
//         {
//             BpBinder *proxy = (BpBinder*)mIn.readPointer();
//             proxy->sendObituary();
//             mOut.writeInt32(BC_DEAD_BINDER_DONE);
//             mOut.writePointer((uintptr_t)proxy);
//         } break;

//     case BR_CLEAR_DEATH_NOTIFICATION_DONE:
//         {
//             BpBinder *proxy = (BpBinder*)mIn.readPointer();
//             proxy->getWeakRefs()->decWeak(proxy);
//         } break;

//     case BR_FINISHED:
//         result = TIMED_OUT;
//         break;

//     case BR_NOOP:
//         break;

//     case BR_SPAWN_LOOPER:
//         mProcess->spawnPooledThread(false);
//         break;

//     default:
//         ALOGE("*** BAD COMMAND %d received from Binder driver\n", cmd);
//         result = UNKNOWN_ERROR;
//         break;
//     }

//     if (result != NO_ERROR) {
//         mLastError = result;
//     }

//     return result;
// }


// status_t IPCThreadState::waitForResponse(Parcel *reply, status_t *acquireResult)
// {
//     uint32_t cmd;
//     int32_t err;

//     while (1) {
//         if ((err=talkWithDriver()) < NO_ERROR) break;
//         err = mIn.errorCheck();
//         if (err < NO_ERROR) break;
//         if (mIn.dataAvail() == 0) continue;

//         cmd = (uint32_t)mIn.readInt32();

//         IF_LOG_COMMANDS() {
//             alog << "Processing waitForResponse Command: "
//                 << getReturnString(cmd) << endl;
//         }

//         switch (cmd) {
//         case BR_ONEWAY_SPAM_SUSPECT:
//             ALOGE("Process seems to be sending too many oneway calls.");
//             CallStack::logStack("oneway spamming", CallStack::getCurrent().get(),
//                     ANDROID_LOG_ERROR);
//             [[fallthrough]];
//         case BR_TRANSACTION_COMPLETE:
//             if (!reply && !acquireResult) goto finish;
//             break;

//         case BR_DEAD_REPLY:
//             err = DEAD_OBJECT;
//             goto finish;

//         case BR_FAILED_REPLY:
//             err = FAILED_TRANSACTION;
//             goto finish;

//         case BR_FROZEN_REPLY:
//             err = FAILED_TRANSACTION;
//             goto finish;

//         case BR_ACQUIRE_RESULT:
//             {
//                 ALOG_ASSERT(acquireResult != NULL, "Unexpected brACQUIRE_RESULT");
//                 const int32_t result = mIn.readInt32();
//                 if (!acquireResult) continue;
//                 *acquireResult = result ? NO_ERROR : INVALID_OPERATION;
//             }
//             goto finish;

//         case BR_REPLY:
//             {
//                 binder_transaction_data tr;
//                 err = mIn.read(&tr, sizeof(tr));
//                 ALOG_ASSERT(err == NO_ERROR, "Not enough command data for brREPLY");
//                 if (err != NO_ERROR) goto finish;

//                 if (reply) {
//                     if ((tr.flags & TF_STATUS_CODE) == 0) {
//                         reply->ipcSetDataReference(
//                             reinterpret_cast<const uint8_t*>(tr.data.ptr.buffer),
//                             tr.data_size,
//                             reinterpret_cast<const binder_size_t*>(tr.data.ptr.offsets),
//                             tr.offsets_size/sizeof(binder_size_t),
//                             freeBuffer);
//                     } else {
//                         err = *reinterpret_cast<const status_t*>(tr.data.ptr.buffer);
//                         freeBuffer(nullptr,
//                             reinterpret_cast<const uint8_t*>(tr.data.ptr.buffer),
//                             tr.data_size,
//                             reinterpret_cast<const binder_size_t*>(tr.data.ptr.offsets),
//                             tr.offsets_size/sizeof(binder_size_t));
//                     }
//                 } else {
//                     freeBuffer(nullptr,
//                         reinterpret_cast<const uint8_t*>(tr.data.ptr.buffer),
//                         tr.data_size,
//                         reinterpret_cast<const binder_size_t*>(tr.data.ptr.offsets),
//                         tr.offsets_size/sizeof(binder_size_t));
//                     continue;
//                 }
//             }
//             goto finish;

//         default:
//             err = executeCommand(cmd);
//             if (err != NO_ERROR) goto finish;
//             break;
//         }
//     }

// finish:
//     if (err != NO_ERROR) {
//         if (acquireResult) *acquireResult = err;
//         if (reply) reply->setError(err);
//         mLastError = err;
//     }

//     return err;
// }

