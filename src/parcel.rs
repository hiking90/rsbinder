use std::vec::Vec;
use crate::error::Result;

macro_rules! read_primitive {
    ( $name:ident, $ty:ty ) => (
        pub fn $name(&mut self) -> Result<$ty> {
            const SIZE: usize = std::mem::size_of::<$ty>();
            let bytes: [u8; SIZE] = (&self.data[self.pos..(self.pos + SIZE)]).try_into()?;
            self.pos += SIZE;
            Ok(<$ty>::from_ne_bytes(bytes))
        }
    )
}

macro_rules! write_primitive {
    ( $name:ident, $ty:ty ) => (
        pub fn $name(&mut self, val: $ty) {
            self.data.extend_from_slice(&val.to_ne_bytes());
        }
    )
}


pub struct Parcel {
    data: Vec<u8>,
    pos: usize,
}

impl Parcel {
    pub fn new(capacity: usize) -> Self {
        Parcel {
            data: Vec::with_capacity(capacity),
            pos: 0,
        }
    }

    pub fn as_mut_data(&mut self) -> &mut Vec<u8> {
        &mut self.data
    }

    pub fn is_empty(&self) -> bool {
        self.pos >= self.data.len()
    }

    pub fn set_data_position(&mut self, pos: usize) {
        self.pos = pos;
    }

    pub fn data_avail(&self) -> usize {
        let result = self.data.len() - self.pos;
        assert!(result < i32::MAX as _, "data too big: {}", result);

        result
    }

    pub fn dump(&self) {
        println!("Parcel: pos {}, len {}, {:?}", self.pos, self.data.len(), self.data);
    }

    read_primitive!(read_f32, f32);
    read_primitive!(read_f64, f64);
    read_primitive!(read_i32, i32);
    read_primitive!(read_u32, u32);
    read_primitive!(read_i64, i64);
    read_primitive!(read_u64, u64);

    pub fn read_byte(&mut self) -> Result<u8> {
        let res = self.read_i32()?;
        Ok(res as _)
    }

    pub fn read_char(&mut self) -> Result<u16> {
        let res = self.read_i32()?;
        Ok(res as _)
    }

    pub fn read_bool(&mut self) -> Result<bool> {
        let res = self.read_i32()?;
        Ok(res != 0)
    }

    write_primitive!(write_i32, i32);
    write_primitive!(write_u32, u32);
    write_primitive!(write_i64, i64);
    write_primitive!(write_u64, u64);
    write_primitive!(write_f32, f32);
    write_primitive!(write_f64, f64);

    pub fn write_byte(&mut self, val: u8) {
        let val: i32 = val as _;
        self.data.extend_from_slice(&val.to_ne_bytes())
    }

    pub fn write_char(&mut self, val: u16) {
        let val: i32 = val as _;
        self.data.extend_from_slice(&val.to_ne_bytes())
    }

    pub fn write_bool(&mut self, val: bool) {
        let val: i32 = val as _;
        self.data.extend_from_slice(&val.to_ne_bytes())
    }

}

#[cfg(test)]
mod tests {
    use crate::*;

    #[test]
    fn test_primitives() -> Result<()> {
        let mut parcel = Parcel::new(10);

        let v_i32:i32 = 1234;
        let v_f32:f32 = 1234.0;
        let v_u32:u32 = 1234;
        let v_i64:i64 = 1234;
        let v_u64:u64 = 1234;
        let v_f64:f64 = 1234.0;

        parcel.write_i32(v_i32);
        parcel.write_f32(v_f32);
        parcel.write_u32(v_u32);
        parcel.write_i64(v_i64);
        parcel.write_u64(v_u64);
        parcel.write_f64(v_f64);

        assert_eq!(parcel.read_i32()?, v_i32);
        assert_eq!(parcel.read_f32()?, v_f32);
        assert_eq!(parcel.read_u32()?, v_u32);
        assert_eq!(parcel.read_i64()?, v_i64);
        assert_eq!(parcel.read_u64()?, v_u64);
        assert_eq!(parcel.read_f64()?, v_f64);

        Ok(())
    }

    #[test]
    fn test_with_slice() -> Result<()> {
        let mut parcel = Parcel::new(256);
        parcel.as_mut_data().extend_from_slice(&[12, 114, 0, 0, 2, 114, 64, 128, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 71, 78, 80, 95, 16, 0, 0, 0, 242, 13, 0, 0, 232, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 96, 209, 234, 37, 127, 0, 0, 0, 96, 209, 234, 37, 127, 0, 0]);
        assert_eq!(parcel.read_i32()?, 29196);

        Ok(())
    }

}


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
