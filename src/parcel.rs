
use std::vec::Vec;

use std::default::Default;


use pretty_hex::*;

use crate::error::{Result, Error, ErrorKind};
use crate::sys::binder::{binder_size_t, flat_binder_object};


use crate::parcelable::*;

const STRICT_MODE_PENALTY_GATHER: i32 = 1 << 31;

#[inline]
fn pad_size(len: usize) -> usize {
    (len+3) & (!3)
}

enum ParcelData<T: Clone + Default + 'static> {
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

    // fn as_mut_slice(&mut self) -> &[T] {
    //     match self {
    //         ParcelData::Vec(ref mut v) => v.as_mut_slice(),
    //         ParcelData::Slice(s) => s,
    //     }
    // }

    fn as_ptr(&self) -> *const T {
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

    fn len(&self) -> usize {
        self.as_slice().len()
    }

    fn resize(&mut self, len: usize) {
        match self {
            ParcelData::Vec(v) => v.resize(len, T::default()),
            _ => panic!("&[u8] can't be resized."),
        }
    }

    fn capacity(&self) -> usize {
        match self {
            ParcelData::Vec(v) => v.capacity(),
            ParcelData::Slice(s) => s.len(),
        }
    }

    fn extend_from_slice(&mut self, other: &[T]) {
        match self {
            ParcelData::Vec(v) => v.extend_from_slice(other),
            _ => panic!("extend_from_slice() is only available for ParcelData::Vec."),
        }
    }

    fn push(&mut self, other: T) {
        match self {
            ParcelData::Vec(v) => v.push(other),
            _ => panic!("extend_from_slice() is only available for ParcelData::Vec."),
        }
    }
}

pub struct Parcel {
    data: ParcelData<u8>,
    objects: ParcelData<binder_size_t>,
    pos: usize,
    next_object_hint: usize,
    request_header_present: bool,
    work_source_request_header_pos: usize,
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
        }
    }

    pub fn from_ipc_parts(data: *mut u8, length: usize,
            objects: *mut binder_size_t, object_count: usize) -> Self {
        Parcel {
            data: ParcelData::from_raw_parts_mut(data, length),
            objects: ParcelData::from_raw_parts_mut(objects, object_count),
            pos: 0,
            next_object_hint: 0,
            request_header_present: false,
            work_source_request_header_pos: 0,
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

    pub fn set_len(&mut self, new_len: usize) {
        self.data.resize(new_len)
    }

    // pub fn close_file_descriptors(&self) {
    //     for offset in &self.objects {
    //         unsafe {
    //             let flat: *const flat_binder_object = self.data.as_ptr().add(*offset as _) as _;
    //             if (*flat).hdr.type_ == BINDER_TYPE_FD {
    //                 libc::close((*flat).__bindgen_anon_1.handle as _);
    //             }
    //         }
    //     }
    // }

    pub fn dump(&self) {
        println!("Parcel: pos {}, len {}", self.pos, self.data.len());
        println!("{}", pretty_hex(&self.data.as_slice()));
    }

    pub fn set_data_position(&mut self, pos: usize) {
        self.pos = pos;
    }

    /// Read a type that implements [`Deserialize`] from the sub-parcel.
    pub fn read<D: Deserialize>(&mut self) -> Result<D> {
        let result = D::deserialize(self);
        result
    }

    pub fn len(&self) -> usize {
        let result = self.data.len() - self.pos;
        assert!(result < i32::MAX as _, "data too big: {}", result);

        result
    }

    pub(crate) fn read_data(&mut self, len: usize) -> Result<&[u8]> {
        let len = pad_size(len);
        let pos = self.pos;

        if len <= self.len() {
            self.pos = pos + len;
            Ok(&self.data.as_slice()[pos .. pos + len])
        } else {
            Err(ErrorKind::NotEnoughData.into())
        }
    }

    pub(crate) fn read_object(&mut self, null_meta: bool) -> Result<flat_binder_object> {
        let data_pos = self.pos as u64;
        let size = std::mem::size_of::<flat_binder_object>();
        let obj = unsafe { *(self.read_data(size)?.as_ptr() as *const flat_binder_object) };

        unsafe {
            if null_meta == false && obj.cookie == 0 && obj.__bindgen_anon_1.binder == 0 {
                return Ok(obj);
            }
        }

        let objects = self.objects.as_slice();
        let count = objects.len();
        let mut opos = self.next_object_hint;

        if count > 0 {
            if opos < count {
                while opos < (count - 1) && (objects[opos] as u64) < data_pos {
                    opos += 1;
                }
            } else {
                opos = count - 1;
            }

            if objects[opos] as u64 == data_pos {
                self.next_object_hint = opos + 1;
                return Ok(obj);
            }

            while opos > 0 && u64::from(objects[opos]) > data_pos {
                opos -= 1;
            }

            if u64::from(objects[opos]) == data_pos {
                self.next_object_hint = opos + 1;
                return Ok(obj);
            }
        }

        Err(Error::from(ErrorKind::BadType))
    }

    pub(crate) fn update_work_source_request_header_pos(&mut self) {
        if self.request_header_present == false {
            self.work_source_request_header_pos = self.data.len();
            self.request_header_present = true;
        }
    }

    pub fn write<S: Serialize + ?Sized>(&mut self, parcelable: &S) -> Result<()> {
        parcelable.serialize(self)
    }

    pub(crate) fn write_data(&mut self, data: &[u8]) {
        self.data.extend_from_slice(data)
    }

    pub(crate) fn write_object(&mut self, obj: &flat_binder_object, null_meta: bool) -> Result<()> {
        const SIZE: usize = std::mem::size_of::<flat_binder_object>();
        let data = unsafe {std::mem::transmute::<&flat_binder_object, &[u8; SIZE]>(obj)};
        let data_pos = self.pos;
        self.write_data(data);

        if null_meta == true || unsafe { obj.__bindgen_anon_1.binder } != 0 {
            self.objects.push(data_pos as _);
        }

        Ok(())
    }
}



impl<'a, const N: usize> TryFrom<&mut Parcel> for [u8; N] {
    type Error = Error;

    fn try_from(parcel: &mut Parcel) -> std::result::Result<Self, Self::Error> {
        let data = parcel.read_data(N)?;
        <[u8; N] as TryFrom<&[u8]>>::try_from(data).map_err(|e| {
            Error::from(e)
        })
        // let pos = parcel.inner.pos;
        // if let Some(data) = parcel.inner.data.get(pos .. (pos + N)) {
        //     parcel.inner.pos += N;
        //     <[u8; N] as TryFrom<&[u8]>>::try_from(data).map_err(|e| {
        //         Error::from(e)
        //     })
        // } else {
        //     Err(Error::from(ErrorKind::BadIndex))
        // }
    }
}


// status_t Parcel::writeObject(const flat_binder_object& val, bool nullMetaData)
// {
//     const bool enoughData = (mDataPos+sizeof(val)) <= mDataCapacity;
//     const bool enoughObjects = mObjectsSize < mObjectsCapacity;
//     if (enoughData && enoughObjects) {
// restart_write:
//         *reinterpret_cast<flat_binder_object*>(mData+mDataPos) = val;

//         // remember if it's a file descriptor
//         if (val.hdr.type == BINDER_TYPE_FD) {
//             if (!mAllowFds) {
//                 // fail before modifying our object index
//                 return FDS_NOT_ALLOWED;
//             }
//             mHasFds = mFdsKnown = true;
//         }

//         // Need to write meta-data?
//         if (nullMetaData || val.binder != 0) {
//             mObjects[mObjectsSize] = mDataPos;
//             acquire_object(ProcessState::self(), val, this, &mOpenAshmemSize);
//             mObjectsSize++;
//         }

//         return finishWrite(sizeof(flat_binder_object));
//     }

//     if (!enoughData) {
//         const status_t err = growData(sizeof(val));
//         if (err != NO_ERROR) return err;
//     }
//     if (!enoughObjects) {
//         if (mObjectsSize > SIZE_MAX - 2) return NO_MEMORY; // overflow
//         if ((mObjectsSize + 2) > SIZE_MAX / 3) return NO_MEMORY; // overflow
//         size_t newSize = ((mObjectsSize+2)*3)/2;
//         if (newSize > SIZE_MAX / sizeof(binder_size_t)) return NO_MEMORY; // overflow
//         binder_size_t* objects = (binder_size_t*)realloc(mObjects, newSize*sizeof(binder_size_t));
//         if (objects == nullptr) return NO_MEMORY;
//         mObjects = objects;
//         mObjectsCapacity = newSize;
//     }

//     goto restart_write;
// }


#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use crate::parcelable::String16;
    use crate::*;

    #[test]
    fn test_primitives() -> Result<()> {
        let v_i32:i32 = 1234;
        let v_f32:f32 = 5678.0;
        let v_u32:u32 = 9012;
        let v_i64:i64 = 3456;
        let v_u64:u64 = 7890;
        let v_f64:f64 = 9876.0;

        let v_str: String16 = String16("Hello World".to_string());

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
            assert_eq!(parcel.read::<String16>()?, v_str);
        }

        Ok(())
    }

    #[test]
    fn test_dyn_ibinder() -> Result<()> {
        let proxy: Arc<dyn IBinder> = Arc::new(proxy::Proxy::new(0, Unknown {}));
        let raw = Arc::into_raw(proxy.clone());

        let mut parcel = Parcel::new();

        {
            parcel.write(&raw)?;
        }
        parcel.set_data_position(0);

        let cloned = proxy.clone();
        {
            let restored = parcel.read::<*const dyn IBinder>()?;

            assert_eq!(raw, restored);
            assert_eq!(Arc::strong_count(&cloned), Arc::strong_count(&unsafe {Arc::from_raw(restored)}));
        }

        Ok(())
    }

    #[test]
    fn test_errors() -> Result<()> {
        Ok(())
    }
}


