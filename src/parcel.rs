use std::array::TryFromSliceError;
use std::vec::Vec;
use std::{mem, ptr};
use std::default::Default;
use crate::error::{Result, Error};
use crate::sys::binder::{binder_size_t, flat_binder_object, BINDER_TYPE_FD};
use crate::thread_state;
use crate::binder;
use crate::parcelable::*;

const STRICT_MODE_PENALTY_GATHER: i32 = 1 << 31;


pub struct Parcel {
    data: Vec<u8>,
    pos: usize,
    objects: *mut binder_size_t,
    object_count: usize,
    request_header_present: bool,
    work_source_request_header_pos: usize,
}

impl Parcel {
    pub fn new() -> Self {
        Parcel::with_capacity(256)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Parcel {
            data: Vec::with_capacity(capacity),
            pos: 0,
            objects: ptr::null_mut(),
            object_count: 0,
            request_header_present: false,
            work_source_request_header_pos: 0,
        }
    }


    pub fn from_ipc_parts(data: *mut u8, length: usize,
            objects: *mut binder_size_t, object_count: usize) -> mem::ManuallyDrop<Self> {
        mem::ManuallyDrop::new(
            Parcel {
                data: unsafe { Vec::from_raw_parts(data, length, length) },
                pos: 0,
                objects: objects,
                object_count: object_count,
                request_header_present: false,
                work_source_request_header_pos: 0,
            }
        )
    }

    pub fn from_vec(data: Vec<u8>) -> Self {
        Parcel {
            data: data,
            pos: 0,
            objects: ptr::null_mut(),
            object_count: 0,
            request_header_present: false,
            work_source_request_header_pos: 0,
        }
    }

    pub fn extend_from_slice(&mut self, other: &[u8]) {
        self.data.extend_from_slice(other)
    }

    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.data.as_mut_ptr()
    }

    pub fn as_ptr(&self) -> * const u8 {
        self.data.as_ptr()
    }

    pub fn capacity(&self) -> usize {
        self.data.capacity()
    }

    pub fn len(&self) -> usize {
        let result = self.data.len() - self.pos;
        assert!(result < i32::MAX as _, "data too big: {}", result);

        result
    }

    pub fn is_empty(&self) -> bool {
        self.pos >= self.data.len()
    }

    pub fn set_len(&mut self, new_len: usize) {
        unsafe { self.data.set_len(new_len); }
    }

    fn update_work_source_request_header_pos(&mut self) {
        if self.request_header_present == false {
            self.work_source_request_header_pos = self.data.len();
            self.request_header_present = true;
        }
    }

    pub fn set_data_position(&mut self, pos: usize) {
        self.pos = pos;
    }

    pub fn close_file_descriptors(&self) {
        for i in 0..self.object_count {
            unsafe {
                let offset = self.objects.add(i);
                let flat: *const flat_binder_object = self.data.as_ptr().add(*offset as _) as _;

                if (*flat).hdr.type_ == BINDER_TYPE_FD {
                    libc::close((*flat).__bindgen_anon_1.handle as _);
                }
            }
        }
    }

    pub fn dump(&self) {
        println!("Parcel: pos {}, len {}, {:?}", self.pos, self.data.len(), self.data);
    }

    pub fn as_readable(&mut self) -> ReadableParcel<'_> {
        ReadableParcel {
            parcel: self,
        }
    }

    pub fn as_writable(&mut self) -> WritableParcel<'_> {
        WritableParcel {
            parcel: self,
        }
    }
}

pub struct ReadableParcel<'a> {
    parcel: &'a mut Parcel,
}

impl<'a> ReadableParcel<'a> {
    /// Read a type that implements [`Deserialize`] from the sub-parcel.
    pub fn read<D: Deserialize>(&mut self) -> Result<D> {
        let size = std::mem::size_of::<D>();
        let result = D::deserialize(self);
        self.parcel.pos += size;
        result
    }

    // /// Check if the sub-parcel has more data to read
    // pub fn has_more_data(&self) -> bool {
    //     self.parcel.get_data_position() < self.end_position
    // }
}

impl<'a, const N: usize> TryFrom<&ReadableParcel<'a>> for [u8; N] {
    type Error = TryFromSliceError;

    fn try_from(parcel: &ReadableParcel<'a>) -> std::result::Result<Self, Self::Error> {
        <[u8; N] as TryFrom<&[u8]>>::try_from(&parcel.parcel.data[parcel.parcel.pos .. (parcel.parcel.pos + N)])
    }
}

pub struct WritableParcel<'a> {
    parcel: &'a mut Parcel,
}

impl<'a> WritableParcel<'a> {
    pub fn write<S: Serialize + ?Sized>(&mut self, parcelable: &S) -> Result<()> {
        parcelable.serialize(self)
    }

    pub fn write_byte_array(&mut self, other: &[u8]) {
        self.parcel.data.extend_from_slice(other)
    }
}


#[cfg(test)]
mod tests {
    use crate::*;

    #[test]
    fn test_primitives() -> Result<()> {
        let v_i32:i32 = 1234;
        let v_f32:f32 = 1234.0;
        let v_u32:u32 = 1234;
        let v_i64:i64 = 1234;
        let v_u64:u64 = 1234;
        let v_f64:f64 = 1234.0;

        let mut parcel = Parcel::new();

        {
            let mut writer = parcel.as_writable();
            writer.write::<i32>(&v_i32);
            writer.write::<u32>(&v_u32);
            writer.write::<f32>(&v_f32);
            writer.write::<i64>(&v_i64);
            writer.write::<u64>(&v_u64);
            writer.write::<f64>(&v_f64);
        }

        {
            let mut reader = parcel.as_readable();
            assert_eq!(reader.read::<i32>()?, v_i32);
            assert_eq!(reader.read::<u32>()?, v_u32);
            assert_eq!(reader.read::<f32>()?, v_f32);
            assert_eq!(reader.read::<i64>()?, v_i64);
            assert_eq!(reader.read::<u64>()?, v_u64);
            assert_eq!(reader.read::<f64>()?, v_f64);
        }

        Ok(())
    }
}


// impl Reader {
//     pub fn new(capacity: usize) -> Self {
//         Reader {
//             data: Vec::with_capacity(capacity),
//             pos: 0,
//             objects: ptr::null_mut(),
//             object_count: 0,
//         }
//     }

//     pub fn from_ipc_data(data: *mut u8, length: usize,
//             objects: *mut binder_size_t, object_count: usize) -> mem::ManuallyDrop<Self> {
//         mem::ManuallyDrop::new(
//             Reader {
//                 data: unsafe { Vec::from_raw_parts(data, length, length) },
//                 pos: 0,
//                 objects: objects,
//                 object_count: object_count,
//             }
//         )
//     }

//     pub fn from_slice(data: &[u8]) -> Self {
//         Reader {
//             data: data.to_vec(),
//             pos: 0,
//             objects: ptr::null_mut(),
//             object_count: 0,
//         }
//     }

//     pub fn from_vec_u8(data: Vec<u8>) -> Self {
//         Reader {
//             data: data,
//             pos: 0,
//             objects: ptr::null_mut(),
//             object_count: 0,
//         }
//     }

//     pub fn into_writer(self) -> Result<Writer> {
//         Ok(Writer {
//             data: self.data
//         })
//     }

//     pub fn set_data_position(&mut self, pos: usize) {
//         self.pos = pos;
//     }

//     pub fn close_file_descriptors(&self) {
//         for i in 0..self.object_count {
//             unsafe {
//                 let offset = self.objects.add(i);
//                 let flat: *const flat_binder_object = self.data.as_ptr().add(*offset as _) as _;

//                 if (*flat).hdr.type_ == BINDER_TYPE_FD {
//                     libc::close((*flat).__bindgen_anon_1.handle as _);
//                 }
//             }
//         }
//     }

//     pub fn dump(&self) {
//         println!("Parcel: pos {}, len {}, {:?}", self.pos, self.data.len(), self.data);
//     }

//     pub fn check_interface(&self, binder: &dyn binder::IBinder) {

//     }

//     read_primitive!(read_f32, f32);
//     read_primitive!(read_f64, f64);
//     read_primitive!(read_i32, i32);
//     read_primitive!(read_u32, u32);
//     read_primitive!(read_i64, i64);
//     read_primitive!(read_u64, u64);

//     pub fn read_byte(&mut self) -> Result<u8> {
//         let res = self.read_i32()?;
//         Ok(res as _)
//     }

//     pub fn read_char(&mut self) -> Result<u16> {
//         let res = self.read_i32()?;
//         Ok(res as _)
//     }

//     pub fn read_bool(&mut self) -> Result<bool> {
//         let res = self.read_i32()?;
//         Ok(res != 0)
//     }

//     pub fn read<T: Copy>(&mut self, size: usize) -> Result<T> {
//         let res: T = unsafe {
//             let ptr: *const T = std::mem::transmute(self.data[self.pos..(self.pos + size)].as_ptr());
//             *ptr
//         };
//         self.pos += size;

//         Ok(res)
//     }
// }

// pub struct Writer {
//     data: Vec<u8>,
// }

// impl Parcel for Writer {
//     fn as_mut_ptr(&mut self) -> *mut u8 {
//         self.data.as_mut_ptr()
//     }

//     fn capacity(&self) -> usize {
//         self.data.capacity()
//     }

//     fn len(&self) -> usize {
//         self.data.len()
//     }

//     fn is_empty(&self) -> bool {
//         self.data.is_empty()
//     }

//     fn set_len(&mut self, new_len: usize) {
//         unsafe { self.data.set_len(new_len); }
//     }
// }

// impl Writer {
//     pub fn new(capacity: usize) -> Self {
//         Writer {
//             data: Vec::with_capacity(capacity),
//         }
//     }

//     pub fn into_reader(self) -> Reader {
//         Reader::from_vec_u8(self.data)
//     }

//     pub fn dump(&self) {
//         println!("Parcel: len {}, {:?}", self.data.len(), self.data);
//     }

//     pub fn extend_from_slice(&mut self, other: &[u8]) {
//         self.data.extend_from_slice(other)
//     }

//     // fn update_work_source_request_header_pos(&mut self) {
//     //     if self.request_header_present == false {
//     //         self.work_source_request_header_pos = self.data.len();
//     //         self.request_header_present = true;
//     //     }
//     // }

//     write_primitive!(write_i16, i16);
//     write_primitive!(write_u16, u16);
//     write_primitive!(write_i32, i32);
//     write_primitive!(write_u32, u32);
//     write_primitive!(write_i64, i64);
//     write_primitive!(write_u64, u64);
//     write_primitive!(write_f32, f32);
//     write_primitive!(write_f64, f64);

//     pub fn write_byte(&mut self, val: u8) {
//         let val: i32 = val as _;
//         self.data.extend_from_slice(&val.to_ne_bytes())
//     }

//     pub fn write_char(&mut self, val: u16) {
//         let val: i32 = val as _;
//         self.data.extend_from_slice(&val.to_ne_bytes())
//     }

//     pub fn write_bool(&mut self, val: bool) {
//         let val: i32 = val as _;
//         self.data.extend_from_slice(&val.to_ne_bytes())
//     }

//     pub fn write_string16(&mut self, val: &str) {
//         self.write_i32(val.len() as _);
//         for ch16 in val.encode_utf16() {
//             self.write_u16(ch16);
//         }
//     }

//     pub fn write_interface_token(&mut self, val: &str) {
//         thread_state::THREAD_STATE.with(|thread_state| {
//             self.write_i32(thread_state.borrow().strict_mode_policy() | STRICT_MODE_PENALTY_GATHER);
//     //     updateWorkSourceRequestHeaderPosition();
//     //     writeInt32(threadState->shouldPropagateWorkSource() ? threadState->getCallingWorkSourceUid()
//     //                                                         : IPCThreadState::kUnsetWorkSource);
//             self.write_i32(-1);
//             self.write_i32(binder::INTERFACE_HEADER as _);
//         });

//         self.write_string16(val);
//     }

//     pub fn write(&mut self, data: &[u8]) {
//         self.data.extend_from_slice(data);
//     }
// }

// #[cfg(test)]
// mod tests {
//     use crate::*;

//     #[test]
//     fn test_primitives() -> Result<()> {
//         let mut writer = parcel::Writer::new(10);

//         let v_i32:i32 = 1234;
//         let v_f32:f32 = 1234.0;
//         let v_u32:u32 = 1234;
//         let v_i64:i64 = 1234;
//         let v_u64:u64 = 1234;
//         let v_f64:f64 = 1234.0;

//         writer.write_i32(v_i32);
//         writer.write_f32(v_f32);
//         writer.write_u32(v_u32);
//         writer.write_i64(v_i64);
//         writer.write_u64(v_u64);
//         writer.write_f64(v_f64);

//         let mut reader = writer.into_reader();

//         assert_eq!(reader.read_i32()?, v_i32);
//         assert_eq!(reader.read_f32()?, v_f32);
//         assert_eq!(reader.read_u32()?, v_u32);
//         assert_eq!(reader.read_i64()?, v_i64);
//         assert_eq!(reader.read_u64()?, v_u64);
//         assert_eq!(reader.read_f64()?, v_f64);

//         Ok(())
//     }

//     #[test]
//     fn test_with_slice() -> Result<()> {
//         let mut reader = parcel::Reader::from_slice(&[12, 114, 0, 0, 2, 114, 64, 128, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 71, 78, 80, 95, 16, 0, 0, 0, 242, 13, 0, 0, 232, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 96, 209, 234, 37, 127, 0, 0, 0, 96, 209, 234, 37, 127, 0, 0]);
//         assert_eq!(reader.read_i32()?, 29196);

//         Ok(())
//     }

// }


// template<class T>
// status_t Parcel::writeAligned(T val) {
//     static_assert(PAD_SIZE_UNSAFE(sizeof(T)) == sizeof(T));

//     if ((mDataPos+sizeof(val)) <= mDataCapacity) {
// restart_write:
//         *reinterpret_cast<T*>(mData+mDataPos) = val;
//         return finishWrite(sizeof(val));
//     }

//     status_t err = growData(sizeof(val));
//     if (err == NO_ERROR) goto restart_write;
//     return err;
// }

// status_t Parcel::writeInterfaceToken(const char16_t* str, size_t len) {
//     if (CC_LIKELY(!isForRpc())) {
//         const IPCThreadState* threadState = IPCThreadState::self();
//         writeInt32(threadState->getStrictModePolicy() | STRICT_MODE_PENALTY_GATHER);
//         updateWorkSourceRequestHeaderPosition();
//         writeInt32(threadState->shouldPropagateWorkSource() ? threadState->getCallingWorkSourceUid()
//                                                             : IPCThreadState::kUnsetWorkSource);
//         writeInt32(kHeader);
//     }

//     // currently the interface identification token is just its name as a string
//     return writeString16(str, len);
// }

// bool Parcel::enforceInterface(const char16_t* interface,
//                               size_t len,
//                               IPCThreadState* threadState) const
// {
//     if (CC_LIKELY(!isForRpc())) {
//         // StrictModePolicy.
//         int32_t strictPolicy = readInt32();
//         if (threadState == nullptr) {
//             threadState = IPCThreadState::self();
//         }
//         if ((threadState->getLastTransactionBinderFlags() & IBinder::FLAG_ONEWAY) != 0) {
//             // For one-way calls, the callee is running entirely
//             // disconnected from the caller, so disable StrictMode entirely.
//             // Not only does disk/network usage not impact the caller, but
//             // there's no way to communicate back violations anyway.
//             threadState->setStrictModePolicy(0);
//         } else {
//             threadState->setStrictModePolicy(strictPolicy);
//         }
//         // WorkSource.
//         updateWorkSourceRequestHeaderPosition();
//         int32_t workSource = readInt32();
//         threadState->setCallingWorkSourceUidWithoutPropagation(workSource);
//         // vendor header
//         int32_t header = readInt32();
//         if (header != kHeader) {
//             ALOGE("Expecting header 0x%x but found 0x%x. Mixing copies of libbinder?", kHeader,
//                   header);
//             return false;
//         }
//     }

//     // Interface descriptor.
//     size_t parcel_interface_len;
//     const char16_t* parcel_interface = readString16Inplace(&parcel_interface_len);
//     if (len == parcel_interface_len &&
//             (!len || !memcmp(parcel_interface, interface, len * sizeof (char16_t)))) {
//         return true;
//     } else {
//         ALOGW("**** enforceInterface() expected '%s' but read '%s'",
//               String8(interface, len).string(),
//               String8(parcel_interface, parcel_interface_len).string());
//         return false;
//     }
// }
