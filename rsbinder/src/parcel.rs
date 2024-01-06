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

use std::vec::Vec;
use std::default::Default;

use pretty_hex::*;
use zerocopy::AsBytes;

use crate::{
    error::{Result, StatusCode},
    sys::binder::{binder_size_t, flat_binder_object},
    sys::{binder_uintptr_t, BINDER_TYPE_FD},
    parcelable::*,
    thread_state,
    binder,
};

const STRICT_MODE_PENALTY_GATHER: i32 = 1 << 31;

#[inline]
pub(crate) fn pad_size(len: usize) -> usize {
    (len+3) & (!3)
}

pub(crate) enum ParcelData<T: Clone + Default + 'static> {
    Vec(Vec<T>),
    Slice(&'static mut [T]),
}

impl<T: Clone + Default> ParcelData<T> {
    fn new() -> Self {
        ParcelData::Vec(Vec::new())
    }

    fn with_capacity(capacity: usize) -> Self {
        ParcelData::Vec(Vec::with_capacity(capacity))
    }

    fn from_vec(data: Vec<T>) -> Self {
        ParcelData::Vec(data)
    }

    fn from_raw_parts_mut(data: *mut T, len: usize) -> Self {
        ParcelData::Slice(unsafe { std::slice::from_raw_parts_mut(data, len) })
    }

    fn as_slice(&self) -> &[T] {
        match self {
            ParcelData::Vec(v) => v.as_slice(),
            ParcelData::Slice(s) => s,
        }
    }

    pub(crate) fn as_ptr(&self) -> *const T {
        match self {
            ParcelData::Vec(ref v) => v.as_ptr(),
            ParcelData::Slice(s) => s.as_ptr(),
        }
    }

    fn as_mut_ptr(&mut self) -> *mut T {
        match self {
            ParcelData::Vec(ref mut v) => v.as_mut_ptr(),
            ParcelData::Slice(s) => s.as_mut_ptr(),
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.as_slice().len()
    }

    fn set_len(&mut self, len: usize) {
        match self {
            ParcelData::Vec(v) => unsafe { v.set_len(len) },
            _ => panic!("&[u8] can't support set_len()."),
        }

    }

    fn resize(&mut self, len: usize) {
        match self {
            ParcelData::Vec(v) => v.resize_with(len, Default::default),
            _ => panic!("&[u8] can't support resize()."),
        }
    }

    fn capacity(&self) -> usize {
        match self {
            ParcelData::Vec(v) => v.capacity(),
            ParcelData::Slice(s) => s.len(),
        }
    }

    fn splice<R, I>(&mut self, range: R, replace_with: I) -> std::vec::Splice<'_, I::IntoIter>
    where
        R: std::ops::RangeBounds<usize>,
        I: IntoIterator<Item = T>,
    {
        match self {
            ParcelData::Vec(v) => v.splice(range, replace_with),
            _ => panic!("splice() is only available for ParcelData::Vec."),
        }
    }

    fn push(&mut self, other: T) {
        match self {
            ParcelData::Vec(v) => v.push(other),
            _ => panic!("extend_from_slice() is only available for ParcelData::Vec."),
        }
    }
}

pub type FnFreeBuffer = fn(Option<&Parcel>, binder_uintptr_t, usize, binder_uintptr_t, usize) -> Result<()>;

/// Parcel converts data into a byte stream (serialization), making it transferable. 
/// The receiving side then transforms this byte stream back into its original data form (deserialization).
pub struct Parcel {
    data: ParcelData<u8>,
    pub(crate) objects: ParcelData<binder_size_t>,
    pos: usize,
    next_object_hint: usize,
    request_header_present: bool,
    work_source_request_header_pos: usize,
    free_buffer: Option<FnFreeBuffer>,
}

impl Default for Parcel {
    fn default() -> Self {
        Parcel::with_capacity(256)
    }
}

impl Parcel {
    pub fn new() -> Self {
        Parcel::with_capacity(256)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Parcel {
            data: ParcelData::with_capacity(capacity),
            objects: ParcelData::new(),
            pos: 0,
            next_object_hint: 0,
            request_header_present: false,
            work_source_request_header_pos: 0,
            free_buffer: None,
        }
    }

    pub fn from_ipc_parts(data: *mut u8, length: usize,
            objects: *mut binder_size_t, object_count: usize,
            free_buffer: fn(Option<&Parcel>, binder_uintptr_t, usize, binder_uintptr_t, usize) -> Result<()>) -> Self {
        Parcel {
            data: ParcelData::from_raw_parts_mut(data, length),
            objects: ParcelData::from_raw_parts_mut(objects, object_count),
            pos: 0,
            next_object_hint: 0,
            request_header_present: false,
            work_source_request_header_pos: 0,
            free_buffer: Some(free_buffer),
        }
    }

    pub fn from_vec(data: Vec<u8>) -> Self {
        Parcel {
            data: ParcelData::from_vec(data),
            objects: ParcelData::new(),
            pos: 0,
            next_object_hint: 0,
            // objects: ptr::null_mut(),
            // object_count: 0,
            request_header_present: false,
            work_source_request_header_pos: 0,
            free_buffer: None,
        }
    }


    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.data.as_mut_ptr()
    }

    pub fn as_ptr(&self) -> *const u8 {
        self.data.as_ptr()
    }

    pub fn capacity(&self) -> usize {
        self.data.capacity()
    }

    pub fn is_empty(&self) -> bool {
        self.pos >= self.data.len()
    }

    pub fn set_data_size(&mut self, new_len: usize) {
        self.data.set_len(new_len);
        if new_len < self.pos {
            self.pos = new_len;
        }
    }

    pub fn close_file_descriptors(&self) {
        for offset in self.objects.as_slice() {
            unsafe {
                let obj: flat_binder_object = self.data.as_ptr().add(*offset as _).into();
                if obj.header_type() == BINDER_TYPE_FD {
                    libc::close(obj.handle() as _);
                }
            }
        }
    }

    pub fn set_data_position(&mut self, pos: usize) {
        self.pos = pos;
    }

    pub fn data_position(&self) -> usize {
        self.pos
    }

    pub fn data_size(&self) -> usize {
        if self.data.len() > self.pos {
            self.data.len()
        } else {
            self.pos
        }
    }

    /// Read a type that implements [`Deserialize`] from the sub-parcel.
    pub fn read<D: Deserialize>(&mut self) -> Result<D> {
        D::deserialize(self)
    }

    pub fn data_avail(&self) -> usize {
        let result = self.data.len() - self.pos;
        assert!(result < i32::MAX as _, "data too big: {}", result);

        result
    }

    pub(crate) fn read_aligned_data(&mut self, len: usize) -> Result<&[u8]> {
        let aligned = pad_size(len);
        let pos = self.pos;

        if aligned <= self.data_avail() {
            self.pos = pos + aligned;
            Ok(&self.data.as_slice()[pos .. pos + len])
        } else {
            log::error!("Not enough data to read aligned data.: {aligned} <= {}", self.data_avail());
            Err(StatusCode::NotEnoughData)
        }
    }

    // TODO : Switch the return value to reference likes &flat_binder_object
    pub(crate) fn read_object(&mut self, null_meta: bool) -> Result<flat_binder_object> {
        let data_pos = self.pos as u64;
        let size = std::mem::size_of::<flat_binder_object>();

        let obj: flat_binder_object = self.read_aligned_data(size)?.as_ptr().into();

        if !null_meta && obj.cookie == 0 && obj.pointer() == 0 {
            return Ok(obj);
        }

        let objects = self.objects.as_slice();
        let count = objects.len();
        let mut opos = self.next_object_hint;

        if count > 0 {
            log::trace!("Parcel looking for obj at {}, hint={}", data_pos, opos);
            if opos < count {
                while opos < (count - 1) && objects[opos] < data_pos {
                    opos += 1;
                }
            } else {
                opos = count - 1;
            }
            if objects[opos] == data_pos {
                self.next_object_hint = opos + 1;
                return Ok(obj);
            }

            while opos > 0 && objects[opos] > data_pos {
                opos -= 1;
            }

            if objects[opos] == data_pos {
                self.next_object_hint = opos + 1;
                return Ok(obj);
            }
        }
        log::error!("Parcel: unable to find object at index {}", data_pos);
        Err(StatusCode::BadType)
    }

    pub(crate) fn update_work_source_request_header_pos(&mut self) {
        if !self.request_header_present {
            self.work_source_request_header_pos = self.data.len();
            self.request_header_present = true;
        }
    }

    pub fn write<S: Serialize + ?Sized>(&mut self, parcelable: &S) -> Result<()> {
        parcelable.serialize(self)
    }

    pub(crate) fn write_aligned<T>(&mut self, val: &T) {
        let unaligned = std::mem::size_of::<T>();
        let val_bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(val as *const T as *const u8, unaligned)
        };

        self.write_aligned_data(val_bytes);
    }

    pub(crate) fn write_aligned_data(&mut self, data: &[u8]) {
        let unaligned = data.len();
        self.data.splice(self.pos.., data.iter().cloned());
        let aligned = pad_size(unaligned);
        if aligned > unaligned {
            self.data.resize(self.data.len() + aligned - unaligned);
        }
        self.pos += aligned;
    }

    pub(crate) fn write_object(&mut self, obj: &flat_binder_object, null_meta: bool) -> Result<()> {
        let data_pos = self.pos;
        self.write_aligned(obj);

        if null_meta || obj.pointer() != 0 {
            obj.acquire()?;
            self.objects.push(data_pos as _);
        }

        Ok(())
    }

    pub(crate) fn write_interface_token(&mut self, interface: &str) -> Result<()> {
        self.write(&(thread_state::strict_mode_policy() | STRICT_MODE_PENALTY_GATHER))?;
        self.update_work_source_request_header_pos();
        let work_source: i32 = if thread_state::should_propagate_work_source() {
            thread_state::calling_work_source_uid() as _
        } else {
            thread_state::UNSET_WORK_SOURCE
        };
        self.write(&work_source)?;
        self.write(&binder::INTERFACE_HEADER)?;
        self.write(&interface)?;

        Ok(())
    }

    pub(crate) fn append_all_from(&mut self, other: &mut Parcel) -> Result<()> {
        self.append_from(other, 0, other.data_size())
    }

    pub(crate) fn append_from(&mut self, _other: &mut Parcel, _start: usize, _size: usize) -> Result<()> {
        todo!()
    }

    fn release_objects(&self) {
        if self.objects.len() == 0 {
            return
        }

        for pos in self.objects.as_slice() {
            let obj: flat_binder_object = unsafe { self.data.as_ptr().add(*pos as _).into() };
            obj.release().map_err(|e| log::error!("Parcel: unable to release object: {:?}", e)).ok();
        }
    }
}

impl Drop for Parcel {
    fn drop(&mut self) {
        match self.free_buffer {
            Some(free_buffer) => {
                free_buffer(Some(self),
                    self.data.as_ptr() as _,
                    self.data.len(),
                    self.objects.as_ptr() as _,
                    self.objects.len()).unwrap();
            }
            None => {
                self.release_objects();
            }
        }
    }
}

impl std::fmt::Debug for Parcel {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        writeln!(f, "Parcel: pos {}, len {}", self.pos, self.data.len())?;
        if self.objects.len() > 0 {
            writeln!(f, "Object count {}\n{}", self.objects.len(), pretty_hex(&self.objects.as_slice().as_bytes()))?;
        }
        write!(f, "{}", pretty_hex(&self.data.as_slice()))
    }
}


impl<const N: usize> TryFrom<&mut Parcel> for [u8; N] {
    type Error = StatusCode;

    fn try_from(parcel: &mut Parcel) -> Result<Self> {
        let data = parcel.read_aligned_data(N)?;
        Ok(<[u8; N] as TryFrom<&[u8]>>::try_from(data)?)
    }
}

#[cfg(test)]
mod tests {
    use crate::*;

    #[test]
    fn test_primitives() -> Result<()> {
        let v_i32:i32 = 1234;
        let v_f32:f32 = 5678.0;
        let v_u32:u32 = 9012;
        let v_i64:i64 = 3456;
        let v_u64:u64 = 7890;
        let v_f64:f64 = 9876.0;

        let v_str = "Hello World".to_owned();

        let mut parcel = Parcel::new();

        {
            parcel.write::<i32>(&v_i32)?;
            parcel.write::<u32>(&v_u32)?;
            parcel.write::<f32>(&v_f32)?;
            parcel.write::<i64>(&v_i64)?;
            parcel.write::<u64>(&v_u64)?;
            parcel.write::<f64>(&v_f64)?;

            parcel.write(&v_str)?;
        }

        parcel.set_data_position(0);

        {
            assert_eq!(parcel.read::<i32>()?, v_i32);
            assert_eq!(parcel.read::<u32>()?, v_u32);
            assert_eq!(parcel.read::<f32>()?, v_f32);
            assert_eq!(parcel.read::<i64>()?, v_i64);
            assert_eq!(parcel.read::<u64>()?, v_u64);
            assert_eq!(parcel.read::<f64>()?, v_f64);
            assert_eq!(parcel.read::<String>()?, v_str);
        }

        Ok(())
    }

    // #[test]
    // fn test_dyn_ibinder() -> Result<()> {
    //     let proxy: Arc<Box<dyn IBinder>> = Arc::new(proxy::Proxy::new_unknown(0));
    //     let raw = Arc::into_raw(proxy.clone());

    //     let mut parcel = Parcel::new();

    //     {
    //         parcel.write(&raw)?;
    //     }
    //     parcel.set_data_position(0);

    //     let cloned = proxy.clone();
    //     {
    //         let restored = parcel.read::<*const dyn IBinder>()?;

    //         assert_eq!(raw, restored);
    //         assert_eq!(Arc::strong_count(&cloned), Arc::strong_count(&unsafe {Arc::from_raw(restored)}));
    //     }

    //     Ok(())
    // }

    #[test]
    fn test_errors() -> Result<()> {
        Ok(())
    }
}


