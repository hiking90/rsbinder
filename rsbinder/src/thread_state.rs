// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

/*
 * Copyright (C) 2020 The Android Open Source Project
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *      http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use std::sync::atomic::Ordering;
use std::os::unix::io::AsRawFd;
use std::cell::RefCell;
use log::error;
use std::backtrace::Backtrace;

use crate::{
    parcel::*,
    error::*,
    binder::*,
    process_state::*,
    sys::*,
    binder_object::*,
};

thread_local! {
    static THREAD_STATE: RefCell<ThreadState> = RefCell::new(ThreadState::new());
    static BINDER_DEREFS: RefCell<BinderDerefs> = RefCell::new(BinderDerefs::new());
}

const RETURN_STRINGS: [&str; 21] =
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

const COMMAND_STRINGS: [&str; 17] =
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
pub(crate) const UNSET_WORK_SOURCE: i32 = -1;

#[derive(Debug, Clone, Copy)]
struct TransactionState {
    calling_pid: binder::pid_t,
    _calling_sid: *const u8,
    _calling_uid: binder::uid_t,
    // strict_mode_policy: i32,
    last_transaction_binder_flags: u32,
    work_source: binder::uid_t,
    propagate_work_source: bool,
}

impl TransactionState {
    fn from_transaction_data(data: &binder::binder_transaction_data_secctx) -> Self {
        TransactionState {
            calling_pid: data.transaction_data.sender_pid,
            _calling_sid: data.secctx as _,
            _calling_uid: data.transaction_data.sender_euid,
            // strict_mode_policy: 0,
            last_transaction_binder_flags: data.transaction_data.flags,
            work_source: 0,
            propagate_work_source: false,
        }
    }
}

// To avoid duplicate calls to borrow_mut() on ThreadState,
// separate the data related to binder dereference.
struct BinderDerefs {
    pending_strong_derefs: Vec<(binder_uintptr_t, binder_uintptr_t)>,
    pending_weak_derefs: Vec<(binder_uintptr_t, binder_uintptr_t)>,
    post_strong_derefs: Vec<SIBinder>,
    post_weak_derefs: Vec<WIBinder>,
}

impl BinderDerefs {
    fn new() -> Self {
        BinderDerefs {
            pending_strong_derefs: Vec::new(),
            pending_weak_derefs: Vec::new(),
            post_strong_derefs: Vec::new(),
            post_weak_derefs: Vec::new(),
        }
    }

    fn process_post_write_derefs(&mut self) {
        self.post_weak_derefs.clear();
        self.post_strong_derefs.clear();
    }

    fn process_pending_derefs(&mut self) -> Result<()> {
        // The decWeak()/decStrong() calls may cause a destructor to run,
        // which in turn could have initiated an outgoing transaction,
        // which in turn could cause us to add to the pending refs
        // vectors; so instead of simply iterating, loop until they're empty.
        //
        // We do this in an outer loop, because calling decStrong()
        // may result in something being added to mPendingWeakDerefs,
        // which could be delayed until the next incoming command
        // from the driver if we don't process it now.
        while !self.pending_weak_derefs.is_empty() || !self.pending_strong_derefs.is_empty() {
            for raw_pointer in self.pending_weak_derefs.drain(..) {
                let strong = raw_pointer_to_strong_binder(raw_pointer);
                SIBinder::downgrade(&strong).decrease();
            }

            if let Some(raw_pointer) = self.pending_strong_derefs.pop() {
                let strong = raw_pointer_to_strong_binder(raw_pointer);
                SIBinder::decrease_drop(strong)?;
            }
        }
        Ok(())
    }
}

pub(crate) struct ThreadState {
    in_parcel: Parcel,
    out_parcel: Parcel,
    transaction: Option<TransactionState>,
    strict_mode_policy: i32,
    is_looper: bool,
    is_flushing: bool,
    call_restriction: CallRestriction,
}

impl ThreadState {
    fn new() -> Self {
        ThreadState {
            in_parcel: Parcel::new(),
            out_parcel: Parcel::new(),
            transaction: None,
            strict_mode_policy: 0,
            is_looper: false,
            is_flushing: false,
            call_restriction: ProcessState::as_self().call_restriction(),
        }
    }

    pub(crate) fn set_strict_mode_policy(&mut self, policy: i32) {
        self.strict_mode_policy = policy;
    }

    pub(crate) fn _strict_mode_policy(&self) -> i32 {
        self.strict_mode_policy
    }

    pub(crate) fn last_transaction_binder_flags(&self) -> u32 {
        match self.transaction {
            Some(tr) => tr.last_transaction_binder_flags,
            None => 0,
        }
    }

    fn is_process_pending_derefs(&mut self) -> bool {
        self.in_parcel.data_position() >= self.in_parcel.data_size()
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

    pub(crate) fn set_calling_work_source_uid_without_propagation(&mut self, uid: binder::uid_t) -> i64 {
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

    fn write_transaction_data(&mut self, cmd: u32, mut flags: u32, handle: u32, code: u32, data: &Parcel, status: &i32) -> Result<()> {
        log::trace!("write_transaction_data: {} {flags:X} {handle} {code}\n{:?}", command_to_str(cmd), data);
        // ptr is initialized by zero because ptr(64) and handle(32) size is different.
        let mut target = binder_transaction_data__bindgen_ty_1 {
            ptr: 0,
        };
        target.handle = handle;

        // let all_flags: u32 = FLAG_PRIVATE_VENDOR | FLAG_CLEAR_BUF | FLAG_ONEWAY;
        // if (flags & !all_flags) != 0 {
        //     log::error!("Unrecognized flags sent: {:X}", flags);
        // }
        let tr = if *status == StatusCode::Ok.into() {
            binder_transaction_data {
                target,
                cookie: 0,
                code,
                flags,
                sender_pid: 0,
                sender_euid: 0,
                data_size: data.data_size() as _,
                offsets_size: (data.objects.len() * std::mem::size_of::<binder_size_t>()) as _,
                data: binder_transaction_data__bindgen_ty_2 {
                    ptr: binder_transaction_data__bindgen_ty_2__bindgen_ty_1 {
                        buffer: data.as_ptr() as _,
                        offsets: data.objects.as_ptr() as _,
                    },
                }
            }
        } else {
            flags |= binder::transaction_flags_TF_STATUS_CODE;
            binder_transaction_data {
                target,
                cookie: 0,
                code,
                flags,
                sender_pid: 0,
                sender_euid: 0,
                data_size: std::mem::size_of::<i32>() as _,
                offsets_size: 0,
                data: binder_transaction_data__bindgen_ty_2 {
                    ptr: binder_transaction_data__bindgen_ty_2__bindgen_ty_1 {
                        buffer: status as *const i32 as _,
                        offsets: 0,
                    },
                }
            }
        };

        self.out_parcel.write::<u32>(&cmd)?;
        self.out_parcel.write_aligned(&tr);

        Ok(())
    }
}

pub(crate) fn set_call_restriction(call_restriction: CallRestriction) {
    THREAD_STATE.with(|thread_state| {
        thread_state.borrow_mut().call_restriction = call_restriction;
    })
}

pub(crate) fn call_restriction() -> CallRestriction {
    THREAD_STATE.with(|thread_state| {
        thread_state.borrow().call_restriction
    })
}

pub(crate) fn strict_mode_policy() -> i32 {
    THREAD_STATE.with(|thread_state| {
        thread_state.borrow().strict_mode_policy
    })
}

pub(crate) fn should_propagate_work_source() -> bool {
    THREAD_STATE.with(|thread_state| {
        thread_state.borrow().transaction.map_or(false, |state| state.propagate_work_source)
    })
}

pub(crate) fn calling_work_source_uid() -> binder::uid_t {
    THREAD_STATE.with(|thread_state| {
        thread_state.borrow().transaction.map_or(0, |state| state.work_source)
    })
}


pub(crate) fn _setup_polling() -> Result<()> {
    THREAD_STATE.with(|thread_state| -> Result<()> {
        thread_state.borrow_mut().out_parcel.write::<u32>(&binder::BC_ENTER_LOOPER)
    })?;
    flush_commands()?;
    Ok(())
}

enum UntilResponse {
    Reply,
    TransactionComplete,
    AcquireResult,
}

fn wait_for_response(until: UntilResponse) -> Result<Option<Parcel>> {
    THREAD_STATE.with(|thread_state| -> Result<Option<Parcel>> {
        loop {
            talk_with_driver(true)?;

            if thread_state.borrow().in_parcel.is_empty()  {
                continue;
            }
            let cmd: u32 = thread_state.borrow_mut().in_parcel.read::<i32>()? as _;

            log::trace!("{:?}", return_to_str(cmd));

            match cmd {
                binder::BR_ONEWAY_SPAM_SUSPECT => {
                    log::error!("Process seems to be sending too many oneway calls.");
                    log::error!("{}", Backtrace::capture());

                    if let UntilResponse::TransactionComplete = until {
                        break
                    }
                },
                binder::BR_TRANSACTION_COMPLETE => {
                    if let UntilResponse::TransactionComplete = until {
                        break
                    }
                }
                binder::BR_DEAD_REPLY => {
                    return Err(StatusCode::DeadObject);
                },
                binder::BR_FAILED_REPLY => {
                    log::error!("Received FAILED_REPLY transaction reply for pid {}",
                        thread_state.borrow().transaction.map_or(0, |state| state.calling_pid));
                    return Err(StatusCode::FailedTransaction);
                },
                binder::BR_FROZEN_REPLY => {
                    log::error!("Received FROZEN_REPLY transaction reply for pid {}",
                        thread_state.borrow().transaction.map_or(0, |state| state.calling_pid));
                    return Err(StatusCode::FailedTransaction);
                },
                binder::BR_ACQUIRE_RESULT => {
                    let result = thread_state.borrow_mut().in_parcel.read::<i32>()?;
                    if let UntilResponse::AcquireResult = until {
                        let res = if result != 0 {
                            Ok(None)
                        } else {
                            Err(StatusCode::InvalidOperation)
                        };
                        return res;
                    } else if cfg!(debug_assertions) {
                        panic!("Unexpected BR_ACQUIRE_RESULT");
                    }
                },
                binder::BR_REPLY => {
                    let tr = thread_state.borrow_mut().in_parcel.read::<binder::binder_transaction_data>()?;
                    let (buffer, offsets) = unsafe { (tr.data.ptr.buffer, tr.data.ptr.offsets) };
                    if let UntilResponse::Reply = until {
                        if (tr.flags & transaction_flags_TF_STATUS_CODE) == 0 {
                            let reply = Parcel::from_ipc_parts(buffer as _, tr.data_size as _,
                                offsets as _,
                                (tr.offsets_size as usize) / std::mem::size_of::<binder::binder_size_t>(),
                                free_buffer);
                            return Ok(Some(reply));
                        } else {
                            let status: StatusCode = unsafe { (*(buffer as *const i32)).into() };
                            log::trace!("binder::BR_REPLY ({})", status);
                            free_buffer(None,
                                buffer,
                                tr.data_size as _,
                                offsets,
                                (tr.offsets_size as usize) / std::mem::size_of::<binder_size_t>())?;

                            if status != StatusCode::Ok {
                                log::warn!("binder::BR_REPLY ({})", status);
                                return Err(status)
                            }
                        }
                    } else {
                        free_buffer(None,
                            buffer,
                            tr.data_size as _,
                            offsets,
                            (tr.offsets_size as usize) / std::mem::size_of::<binder_size_t>())?;
                    }
                },
                _ => {
                    execute_command(cmd as _)?;
                }
            };
        };
        Ok(None)
    })
}

fn execute_command(cmd: i32) -> Result<()> {
    let cmd: std::os::raw::c_uint = cmd as _;

    THREAD_STATE.with(|thread_state| -> Result<()> {
        match cmd {
            binder::BR_ERROR => {
                let other: StatusCode = thread_state.borrow_mut().in_parcel.read::<i32>()?.into();
                log::error!("binder::BR_ERROR ({})", other);
                return Err(other);
            }
            binder::BR_OK => {}

            binder::BR_TRANSACTION_SEC_CTX |
            binder::BR_TRANSACTION => {
                let tr_secctx = {
                    let mut thread_state = thread_state.borrow_mut();
                    if cmd == binder::BR_TRANSACTION_SEC_CTX {
                        thread_state.in_parcel.read::<binder::binder_transaction_data_secctx>()?
                    } else {
                        binder::binder_transaction_data_secctx {
                            transaction_data: thread_state.in_parcel.read::<binder::binder_transaction_data>()?,
                            secctx: 0,
                        }
                    }
                };

                let mut reader = unsafe {
                    let tr = &tr_secctx.transaction_data;

                    Parcel::from_ipc_parts(tr.data.ptr.buffer as _, tr.data_size as _,
                        tr.data.ptr.offsets as _, (tr.offsets_size as usize) / std::mem::size_of::<binder::binder_size_t>(),
                        free_buffer)
                };

                // TODO: Skip now, because if below implmentation is mandatory.
                // const void* origServingStackPointer = mServingStackPointer;
                // mServingStackPointer = &origServingStackPointer; // anything on the stack

                let transaction_old = {
                    let mut thread_state = thread_state.borrow_mut();
                    let transaction_old = thread_state.transaction;

                    thread_state.clear_calling_work_source();
                    thread_state.clear_propagate_work_source();

                    thread_state.transaction = Some(TransactionState::from_transaction_data(&tr_secctx));

                    transaction_old
                };

                let mut reply = Parcel::new();

                let result = {
                    let target_ptr = unsafe { tr_secctx.transaction_data.target.ptr };
                    // reader.set_data_position(0);
                    if target_ptr != 0 {
                        let strong = raw_pointer_to_strong_binder((target_ptr, tr_secctx.transaction_data.cookie));
                        if strong.attempt_increase() {
                            let result = strong.as_transactable().expect("Transactable is None.")
                                .transact(tr_secctx.transaction_data.code, &mut reader, &mut reply);
                            strong.decrease()?;

                            result
                        } else {
                            log::warn!("Failed StrongBinder::attempt_increase.");
                            Err(StatusCode::UnknownTransaction)
                        }
                    } else {
                        let context = ProcessState::as_self().context_manager().expect("Transactable is None.");
                        context.as_transactable().expect("Transactable is None.").transact(tr_secctx.transaction_data.code, &mut reader, &mut reply)
                    }
                };
                let flags = tr_secctx.transaction_data.flags;
                if (flags & transaction_flags_TF_ONE_WAY) == 0 {
                    let flags = flags & transaction_flags_TF_CLEAR_BUF;
                    let status: i32 = match result {
                        Ok(_) => StatusCode::Ok.into(),
                        Err(err) => err.into(),
                    };
                    thread_state.borrow_mut().write_transaction_data(binder::BC_REPLY, flags, u32::MAX, 0, &reply, &status)?;
                    // reply.set_data_size(0);
                    wait_for_response(UntilResponse::TransactionComplete)?;
                } else if let Err(err) = result {
                    let mut log = format!(
                        "oneway function results for code {} on binder at {:X}",
                        tr_secctx.transaction_data.code, unsafe { tr_secctx.transaction_data.target.ptr });
                    log += &format!(" will be dropped but finished with status {}", err);

                    if reply.data_size() != 0 {
                        log += &format!(" and reply parcel size {}", reply.data_size());
                    }
                    log::error!("{}", log);
                }

                thread_state.borrow_mut().transaction = transaction_old;
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
                let mut state = thread_state.borrow_mut();
                let refs = state.in_parcel.read::<binder::binder_uintptr_t>()?;
                let obj = state.in_parcel.read::<binder::binder_uintptr_t>()?;

                let strong = raw_pointer_to_strong_binder((refs, obj));
                let weak = SIBinder::downgrade(&strong);
                weak.increase();

                state.out_parcel.write::<u32>(&binder::BC_INCREFS_DONE)?;
                state.out_parcel.write::<binder::binder_uintptr_t>(&refs)?;
                state.out_parcel.write::<binder::binder_uintptr_t>(&obj)?;
            }
            binder::BR_ACQUIRE => {
                let mut state = thread_state.borrow_mut();
                let refs = state.in_parcel.read::<binder::binder_uintptr_t>()?;
                let obj = state.in_parcel.read::<binder::binder_uintptr_t>()?;

                let strong = raw_pointer_to_strong_binder((refs, obj));
                // strong is ManuallyDrop, so increase() is called once.
                strong.increase()?;

                state.out_parcel.write::<u32>(&(binder::BC_ACQUIRE_DONE))?;
                state.out_parcel.write::<binder::binder_uintptr_t>(&refs)?;
                state.out_parcel.write::<binder::binder_uintptr_t>(&obj)?;
            }
            binder::BR_RELEASE => {
                let mut state = thread_state.borrow_mut();
                let refs = state.in_parcel.read::<binder::binder_uintptr_t>()?;
                let obj = state.in_parcel.read::<binder::binder_uintptr_t>()?;

                BINDER_DEREFS.with(|binder_derefs| {
                    let mut binder_derefs = binder_derefs.borrow_mut();
                    binder_derefs.pending_strong_derefs.push((refs, obj));
                });
            }
            binder::BR_DECREFS => {
                let mut state = thread_state.borrow_mut();
                let refs = state.in_parcel.read::<binder::binder_uintptr_t>()?;
                let obj = state.in_parcel.read::<binder::binder_uintptr_t>()?;

                BINDER_DEREFS.with(|binder_derefs| {
                    let mut binder_derefs = binder_derefs.borrow_mut();
                    binder_derefs.pending_weak_derefs.push((refs, obj));
                });
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
                ProcessState::as_self().spawn_pooled_thread(false);
            }
            binder::BR_FINISHED => {
                return Err(StatusCode::TimedOut);
            }
            binder::BR_DEAD_BINDER => {
                let handle = {
                    let mut state = thread_state.borrow_mut();
                    state.in_parcel.read::<binder::binder_uintptr_t>()?
                };

                ProcessState::as_self().send_obituary_for_handle(handle as _)?;

                {
                    let mut state = thread_state.borrow_mut();
                    state.out_parcel.write::<u32>(&(binder::BC_DEAD_BINDER_DONE))?;
                    state.out_parcel.write::<binder::binder_uintptr_t>(&handle)?;
                }
            }
            binder::BR_CLEAR_DEATH_NOTIFICATION_DONE => {
                let mut state = thread_state.borrow_mut();
                state.in_parcel.read::<binder::binder_uintptr_t>()?;
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
            _ => {
                log::error!("*** BAD COMMAND {} received from Binder driver\n", cmd);
                return Err(StatusCode::Unknown);
            }
        };

        Ok(())
    })
}


fn talk_with_driver(do_receive: bool) -> Result<()> {
    let driver = ProcessState::as_self().driver();

    THREAD_STATE.with(|thread_state| -> Result<()> {
        let mut bwr = {
            let mut thread_state = thread_state.borrow_mut();
            let need_read = thread_state.in_parcel.is_empty();
            let out_avail = if !do_receive || need_read {
                thread_state.out_parcel.data_size()
            } else {
                0
            };

            let read_size = if do_receive && need_read {
                thread_state.in_parcel.capacity()
            } else {
                0
            };

            binder::binder_write_read {
                write_size: out_avail as _,
                write_consumed: 0,
                write_buffer: thread_state.out_parcel.as_mut_ptr() as _,
                read_size: read_size as _,
                read_consumed: 0,
                read_buffer: thread_state.in_parcel.as_mut_ptr() as _,
            }
        };

        if bwr.write_size == 0 && bwr.read_size == 0 {
            return Ok(())
        }

        if bwr.write_size != 0 {
            log::trace!("Sending command to driver:\n{:?}", thread_state.borrow().out_parcel);
            log::trace!("Size of receive buffer: {}, need_read: {}, do_receive: {}",
                bwr.read_size, thread_state.borrow().in_parcel.is_empty(), do_receive);
        }

        unsafe {
            loop {
                let res = binder::write_read(driver.as_raw_fd(), &mut bwr);
                match res {
                    Ok(_) => break,
                    Err(errno) if errno != nix::errno::Errno::EINTR => {
                        log::error!("binder::write_read() error : {}", errno);
                        return Err(StatusCode::Errno(-(errno as i32)));
                    },
                    _ => {}
                }

            }
        }

        log::trace!("errno: {:?}, write consumed: {} of {}, read consumed: {}",
            nix::errno::Errno::last(), bwr.write_consumed, bwr.write_size, bwr.read_consumed);

        {
            let mut thread_state = thread_state.borrow_mut();

            if bwr.write_consumed > 0 {
                if bwr.write_consumed < thread_state.out_parcel.data_size() as _ {
                    panic!("Driver did not consume write buffer. consumed: {} of {}",
                        bwr.write_consumed, thread_state.out_parcel.data_size());
                } else {
                    thread_state.out_parcel.set_data_size(0);
                    drop(thread_state);

                    BINDER_DEREFS.with(|binder_derefs| {
                        binder_derefs.borrow_mut().process_post_write_derefs()
                    });
                }
            }
        }
        {
            let mut thread_state = thread_state.borrow_mut();
            if bwr.read_consumed > 0 {
                thread_state.in_parcel.set_data_size(bwr.read_consumed as _);
                thread_state.in_parcel.set_data_position(0);

                log::trace!("Received commands to driver:\n{:?}", thread_state.in_parcel);
            }
        };

        Ok(())
    })
}

fn get_and_execute_command() -> Result<()> {
    talk_with_driver(true)?;

    let cmd = THREAD_STATE.with(|thread_state| -> Result<i32> {
        thread_state.borrow_mut().in_parcel.read::<i32>()
    })?;
    execute_command(cmd)?;

    Ok(())
}

pub(crate) fn flush_commands() -> Result<()> {
    talk_with_driver(false)?;

    THREAD_STATE.with(|thread_state| -> Result<()> {
        if thread_state.borrow().out_parcel.data_size() > 0 {
            talk_with_driver(false)?;
        }

        if thread_state.borrow().out_parcel.data_size() > 0 {
            log::warn!("self.out_parcel.len() > 0 after flash_commands()");
        }

        Ok(())
    })
}

pub(crate) fn attempt_inc_strong_handle(handle: u32) -> Result<()> {
    log::trace!("attempt_inc_strong_handle: {handle}");
    THREAD_STATE.with(|thread_state| -> Result<()> {
        let mut state = thread_state.borrow_mut();

        state.out_parcel.write::<u32>(&(binder::BC_ATTEMPT_ACQUIRE))?;
        state.out_parcel.write::<u32>(&0)?;     // xxx was thread priority.
        state.out_parcel.write::<u32>(&(handle))
    })?;
    wait_for_response(UntilResponse::AcquireResult).map(|_| ())
}

pub(crate) fn inc_strong_handle(handle: u32, proxy: SIBinder) -> Result<()> {
    log::trace!("inc_strong_handle: {handle}");
    THREAD_STATE.with(|thread_state| -> Result<()> {
        {
            let mut state = thread_state.borrow_mut();

            state.out_parcel.write::<u32>(&(binder::BC_ACQUIRE))?;
            state.out_parcel.write::<u32>(&(handle))?;
        }

        if !(flash_if_needed()?) {
            BINDER_DEREFS.with(|binder_derefs| {
                binder_derefs.borrow_mut().post_strong_derefs.push(proxy);
            });
        }

        Ok(())
    })
}

pub(crate) fn dec_strong_handle(handle: u32) -> Result<()> {
    log::trace!("dec_strong_handle: {handle}");
    THREAD_STATE.with(|thread_state| -> Result<()> {
        {
            let mut state = thread_state.borrow_mut();

            state.out_parcel.write::<u32>(&(binder::BC_RELEASE))?;
            state.out_parcel.write::<u32>(&(handle))?;
        }

        flash_if_needed()?;

        Ok(())
    })
}

pub(crate) fn inc_weak_handle(handle: u32, weak: &WIBinder) -> Result<()>{
    log::trace!("inc_weak_handle: {handle}");
    THREAD_STATE.with(|thread_state| -> Result<()> {
        {
            let mut state = thread_state.borrow_mut();

            state.out_parcel.write::<u32>(&(binder::BC_INCREFS))?;
            state.out_parcel.write::<u32>(&(handle))?;
        }

        if !(flash_if_needed()?) {
            // This code is come from IPCThreadState.cpp. Is it necessaryq?
            BINDER_DEREFS.with(|binder_derefs| {
                binder_derefs.borrow_mut().post_weak_derefs.push(weak.clone());
            });
        }

        Ok(())
    })
}

pub(crate) fn dec_weak_handle(handle: u32) -> Result<()> {
    log::trace!("dec_weak_handle: {handle}");
    THREAD_STATE.with(|thread_state| -> Result<()> {
        {
            let mut state = thread_state.borrow_mut();

            state.out_parcel.write::<u32>(&(binder::BC_DECREFS))?;
            state.out_parcel.write::<u32>(&(handle))?;
        }

        flash_if_needed()?;

        Ok(())
    })
}


pub(crate) fn flash_if_needed() -> Result<bool> {
    THREAD_STATE.with(|thread_state| -> Result<bool> {
        {
            let thread_state = thread_state.borrow();
            if thread_state.is_looper || thread_state.is_flushing {
                return Ok(false);
            }
        }

        thread_state.borrow_mut().is_flushing = true;
        flush_commands()?;
        thread_state.borrow_mut().is_flushing = false;

        Ok(true)
    })
}

pub(crate) fn _handle_commands() -> Result<()> {
    while {
        get_and_execute_command()?;

        THREAD_STATE.with(|thread_state| -> bool {
            !thread_state.borrow().in_parcel.is_empty()
        })
    } {
        flush_commands()?;
    }
    Ok(())
}

pub fn check_interface(reader: &mut Parcel, descriptor: &str) -> Result<bool> {
    let mut strict_policy: i32 = reader.read()?;

    let header = THREAD_STATE.with(|thread_state| -> Result<u32> {
        let mut thread_state = thread_state.borrow_mut();

        if (thread_state.last_transaction_binder_flags() & FLAG_ONEWAY) != 0 {
            strict_policy = 0;
        }
        thread_state.set_strict_mode_policy(strict_policy);
        reader.update_work_source_request_header_pos();

        let work_source: i32 = reader.read()?;
        thread_state.set_calling_work_source_uid_without_propagation(work_source as _);

        reader.read()
    })?;

    if header != INTERFACE_HEADER {
        log::error!("Expecting header {:#x} but found {:#x}.", INTERFACE_HEADER, header);
        return Ok(false);
    }

    let parcel_interface: String = reader.read()?;
    if parcel_interface.eq(descriptor) {
        Ok(true)
    } else {
        log::error!("check_interface() expected '{}' but read '{}'", descriptor, parcel_interface);
        Ok(false)
    }
}

pub(crate) fn transact(handle: u32, code: u32, data: &Parcel, mut flags: u32) -> Result<Option<Parcel>> {
    let mut reply: Option<Parcel> = None;

    flags |= transaction_flags_TF_ACCEPT_FDS;

    let call_restriction = THREAD_STATE.with(|thread_state| -> Result<CallRestriction> {
        let mut thread_state = thread_state.borrow_mut();
        thread_state.write_transaction_data(binder::BC_TRANSACTION, flags, handle, code, data, &0)?;
        Ok(thread_state.call_restriction)
    })?;

    if (flags & transaction_flags_TF_ONE_WAY) == 0 {
        match call_restriction {
            CallRestriction::ErrorIfNotOneway => {
                error!("Process making non-oneway call (code: {}) but is restricted.", code)
            },
            CallRestriction::FatalIfNotOneway => {
                panic!("Process may not make non-oneway calls (code: {}).", code);
            },
            _ => (),
        }

        reply = wait_for_response(UntilResponse::Reply)?;
    } else {
        wait_for_response(UntilResponse::TransactionComplete)?;
    }

    Ok(reply)
}


fn free_buffer(parcel: Option<&Parcel>, data: binder_uintptr_t, _: usize, _ : binder_uintptr_t, _: usize) -> Result<()> {
    if let Some(parcel) = parcel {
        parcel.close_file_descriptors()
    }

    THREAD_STATE.with(|thread_state| -> Result<()> {
        let mut thread_state = thread_state.borrow_mut();
        thread_state.out_parcel.write::<u32>(&binder::BC_FREE_BUFFER)?;
        thread_state.out_parcel.write::<binder_uintptr_t>(&data)?;
        Ok(())
    })?;

    flash_if_needed()?;

    Ok(())
}

pub(crate) fn query_interface(handle: u32) -> Result<String> {
    let data = Parcel::new();
    let reply = transact(handle, INTERFACE_TRANSACTION, &data, 0)?;
    let interface: String = reply.expect("INTERFACE_TRANSACTION should have reply parcel").read()?;

    Ok(interface)
}

pub(crate) fn ping_binder(handle: u32) -> Result<()> {
    let data = Parcel::new();
    let _reply = transact(handle, PING_TRANSACTION, &data, 0)?;
    Ok(())
}

pub(crate) fn join_thread_pool(is_main: bool) -> Result<()> {
    THREAD_STATE.with(|thread_state| -> Result<()> {
        log::debug!("**** THREAD {:?} (PID {}) IS JOINING THE THREAD POOL",
            std::thread::current().id(), std::process::id());

        ProcessState::as_self().current_threads.fetch_add(1, Ordering::SeqCst);

        let looper = if is_main {
            binder::BC_ENTER_LOOPER
        } else {
            binder::BC_REGISTER_LOOPER
        };

        {
            let mut thread_state = thread_state.borrow_mut();
            thread_state.out_parcel.write::<u32>(&looper)?;
            thread_state.is_looper = true;
        }

        let result;

        loop {
            if thread_state.borrow_mut().is_process_pending_derefs() {
                BINDER_DEREFS.with(|binder_derefs| -> Result<()> {
                    binder_derefs.borrow_mut().process_pending_derefs()
                })?;
            }
            if let Err(e) = get_and_execute_command() {
                match e {
                    StatusCode::TimedOut if !is_main => {
                        result = e;
                        break
                    }
                    StatusCode::Errno(errno) if errno == (nix::errno::Errno::ECONNREFUSED as i32) => {
                        result = e;
                        break;
                    }
                    _ => {
                        panic!("get_and_execute_command() returned unexpected error {}, aborting", e);
                    }
                }
            }
        }
        log::debug!("**** THREAD {:?} (PID {}) IS LEAVING THE THREAD POOL err={}\n",
            std::thread::current().id(), std::process::id(), result);

        {
            let mut thread_state = thread_state.borrow_mut();

            thread_state.out_parcel.write::<u32>(&binder::BC_EXIT_LOOPER)?;
            thread_state.is_looper = false;
        }

        talk_with_driver(false)?;
        ProcessState::as_self().current_threads.fetch_sub(1, Ordering::SeqCst);
        Ok(())
    })
}

pub(crate) fn request_death_notification(handle: u32) -> Result<()> {
    log::trace!("request_death_notification: {handle}");
    THREAD_STATE.with(|thread_state| -> Result<()> {
        {
            let mut state = thread_state.borrow_mut();

            state.out_parcel.write::<u32>(&(binder::BC_REQUEST_DEATH_NOTIFICATION))?;
            state.out_parcel.write::<u32>(&(handle))?;
            // Android binder calls writePointer(proxy) here, but we just write handle.
            state.out_parcel.write::<binder::binder_uintptr_t>(&(handle as _))?;
        }

        Ok(())
    })
}

pub(crate) fn clear_death_notification(handle: u32) -> Result<()> {
    log::trace!("clear_death_notification: {handle}");
    THREAD_STATE.with(|thread_state| -> Result<()> {
        {
            let mut state = thread_state.borrow_mut();

            state.out_parcel.write::<u32>(&(binder::BC_CLEAR_DEATH_NOTIFICATION))?;
            state.out_parcel.write::<u32>(&(handle))?;
            // Android binder calls writePointer(proxy) here, but we just write handle.
            state.out_parcel.write::<binder::binder_uintptr_t>(&(handle as _))?;
        }

        Ok(())
    })
}

pub struct CallingContext {
    pub pid: binder::pid_t,
    pub uid: binder::uid_t,
    pub sid: *const u8,
}

pub(crate) fn _get_calling_context() -> Result<CallingContext> {
    THREAD_STATE.with(|thread_state| -> Result<CallingContext> {
        let thread_state = thread_state.borrow();
        let transaction = thread_state.transaction.as_ref().ok_or(StatusCode::Unknown)?;
        let calling_pid = transaction.calling_pid;
        let calling_uid = transaction._calling_uid;
        let calling_sid = transaction._calling_sid;

        Ok(CallingContext {
            pid: calling_pid,
            uid: calling_uid,
            sid: calling_sid,
        })
    })
}